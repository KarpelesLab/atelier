//! Minimal terminal interface.
//!
//! The intended interface (roadmap M2): a single input line plus a status strip
//! (model · cwd · git branch · token counters). The input row redraws in place;
//! **everything else — assistant text, reasoning, tool activity — prints to the
//! terminal scrollback and is never redrawn.** No panes, no mouse, no
//! alt-screen. Streamed output must interleave above the live input line
//! without corrupting it. The terminal itself — raw mode, keys, size — is
//! [`term`], atelier's own small layer over it.
//!
//! # Contract (stable — implementers must not change this signature)
//!
//! [`run`] takes ownership of a [`Session`](crate::agent::Session) and drives it
//! to completion, reading user input and rendering through a [`Ui`](crate::agent::Ui)
//! implementation of your own (a `TuiUi`). Handle `/quit`, `/models`, Ctrl-C
//! (cancel the in-flight turn) and Ctrl-D (exit).
//!
//! ## Design & invariant
//!
//! The bottom of the screen holds a *live region* — an optional in-progress
//! output line, a reverse-video status strip, and the input line — which is the
//! **only** thing ever redrawn. Every complete line of model output, reasoning,
//! tool activity, and the user's own submitted prompts is *committed* to the
//! scrollback above the live region exactly once and never touched again.
//!
//! [`Renderer::refresh`] is the single primitive that upholds this: it walks
//! the cursor to the top of the previously-drawn live region, clears from there
//! down, prints any newly-committed lines (which scroll the terminal naturally),
//! then repaints the live region below them and parks the cursor in the input
//! line. Because committed lines are drained and printed once, they become
//! immutable scrollback; only the two/three-row live region is ever rewritten.
//!
//! ## Known limitations (see also `input.rs`)
//!
//! - **Ctrl-C stops a turn at the next opportunity, not instantly.** Each
//!   press does one thing: clear the input line if it has text, else drop
//!   queued messages if any, else stop the running turn ([`Ui::interrupted`]).
//!   The OpenAI loop notices at the next streamed chunk or between tool
//!   calls (a tool already running finishes; a request still waiting for its
//!   first byte isn't cut short). The Claude backend preempts claude's turn
//!   and **never stops its background agents** (see `src/claude`). Ctrl-D on
//!   an empty line exits between turns — twice, if claude has background
//!   work running.
//! - **Terminal height must be ≥ 3 rows.** The live region occupies up to three
//!   rows and the redraw moves the cursor up by up to two; on a 1–2 row terminal
//!   the accounting degrades (no crash, but the strip may be clipped).
//! - Column math is one-per-`char` (no Unicode width); wide/combining glyphs
//!   mis-place the cursor. Mid-turn resizes are absorbed because [`Renderer::refresh`]
//!   re-reads the terminal width every time rather than caching it.
//!
//! ## Working state & queued input
//!
//! A turn runs on a scoped worker thread ([`run_turn`]) that owns the
//! [`Session`] for its duration and reports through a channel-backed [`Ui`]
//! ([`ChanUi`]). The main thread keeps reading the keyboard, so the input line
//! stays live: the status strip shows a spinner (`⠋ working 12s`) and Enter
//! *queues* the line instead of sending it. Queued messages are handed to the
//! agent loop between steps (see [`Ui::take_queued`]) so the model sees them
//! at its next request, and whatever is still queued when the turn ends is
//! served in order as if it had just been typed — commands run, prompts start
//! new turns. Approval prompts arrive over the same channel and are answered
//! from the main thread, so the keyboard is never read by two threads.
//!
//! ## Questions from the model
//!
//! The `ask` tool's question travels the same way ([`UiEvent::AskUser`]): the
//! question and its numbered options are committed to the scrollback like any
//! other lines, and the renderer remembers it as pending
//! ([`Renderer::question`]). While it is, the next line submitted on the
//! input line answers it instead of being queued ([`question_key`]); an
//! answer that can't be understood prints the hint again and keeps waiting,
//! and Ctrl-C on an empty line dismisses the question (the turn goes on, and
//! the model is told the user declined).

mod config;
mod input;
mod term;

use std::collections::VecDeque;
use std::io::{self, IsTerminal, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;

use crate::agent::{self, Session, Ui};
use crate::permissions::Mode;
use crate::tools::ask::{Answer, Question};

use input::LineEditor;
use term::{Event, Key};

/// The input prompt and its width in columns (`›` + space, both single-width).
const PROMPT: &str = "› ";
const PROMPT_COLS: usize = 2;

/// Messages typed while a turn was running, waiting to be delivered — shared
/// between the renderer (which enqueues on Enter and serves leftovers after the
/// turn) and the worker's [`ChanUi`] (which drains it between steps).
type Queue = Arc<Mutex<VecDeque<String>>>;

/// How often the status strip repaints while a turn runs (spinner + elapsed).
const SPINNER_TICK: Duration = Duration::from_millis(100);
/// How long one keyboard poll blocks while a turn runs; this is also the pump
/// loop's sleep, so it bounds output latency.
const KEY_POLL: Duration = Duration::from_millis(30);
/// How long the idle prompt waits for a key before checking for output that
/// arrived between turns (claude reporting on background work).
const IDLE_POLL: Duration = Duration::from_millis(200);
const SPINNER: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

/// Drive a session through the inline terminal interface.
pub fn run(mut session: Session) -> Result<()> {
    // The inline UI needs a real terminal (raw mode, key events). When stdin or
    // stdout is redirected — piped input, captured output, a test harness — fall
    // back to the plain line REPL, which reads stdin to EOF.
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return agent::repl(session);
    }

    // Raw mode from here; the guard restores the terminal on every exit path,
    // including `?` early-returns and panics (its `Drop` runs while unwinding).
    let _guard = RawGuard::enable()?;

    let mut r = Renderer::new();
    let mut turn: u32 = 0;
    r.status = build_status(&session, turn);
    r.push_line("atelier — /help for commands, /quit to exit".into(), true);
    for line in session.connect_configured_mcp() {
        r.push_line(line, true);
    }
    r.refresh()?;

    loop {
        // Whatever was queued during the last turn is served first, in order,
        // exactly as if it had just been typed (echoed, then dispatched).
        let input = match r.pop_queued() {
            Some(line) => {
                r.push_line(format!("{PROMPT}{line}"), false);
                line
            }
            None => match read_line(&mut r, &mut session, turn)? {
                Some(line) => line,
                // Ctrl-D: exits, unless that would stop claude's background
                // work and this is the first attempt.
                None => {
                    if session.confirm_quit(&mut TuiUi { r: &mut r }) {
                        break;
                    }
                    r.refresh()?;
                    continue;
                }
            },
        };
        r.refresh()?;

        let trimmed = input.trim().to_string();
        if trimmed.is_empty() {
            continue;
        }

        // Commands and prompts share one dispatcher. Scope the TuiUi borrow so
        // `r` is free again in the match arms.
        let outcome = {
            let mut ui = TuiUi { r: &mut r };
            agent::dispatch(&mut session, &trimmed, &mut ui)
        };
        match outcome {
            agent::Dispatch::Quit => break,
            agent::Dispatch::Handled => {
                // `/mode` and `/model` change what the strip shows.
                r.status = build_status(&session, turn);
                r.refresh()?;
            }
            agent::Dispatch::Config => {
                config::run(&mut session)?;
                r.status = build_status(&session, turn);
                r.refresh()?;
            }
            outcome @ (agent::Dispatch::Prompt | agent::Dispatch::Expanded(_)) => {
                // A file-defined command sends its expanded template.
                let prompt = match &outcome {
                    agent::Dispatch::Expanded(text) => text,
                    _ => &trimmed,
                };
                turn += 1;
                r.status = build_status(&session, turn);
                run_turn(&mut session, &mut r, prompt)?;
                // Usage counters moved; recompute the strip now that we can
                // borrow the session again.
                r.status = build_status(&session, turn);
                r.refresh()?;
            }
        }
    }
    Ok(())
}

/// Block at the idle prompt until the user submits a line (`Some`) or asks to
/// exit with Ctrl-D on an empty input (`None`). The submitted line is echoed
/// into the scrollback record. Meanwhile, output the session produces on its
/// own (claude reporting on background work) is rendered above the prompt.
fn read_line(r: &mut Renderer, session: &mut Session, turn: u32) -> Result<Option<String>> {
    loop {
        let Some(event) = term::poll(IDLE_POLL)? else {
            if session.poll_background(&mut TuiUi { r: &mut *r }) {
                r.status = build_status(session, turn);
                r.refresh()?;
            }
            // Messages from other atelier instances (see `coord`).
            if session.poll_peers(&mut TuiUi { r: &mut *r }) {
                r.refresh()?;
            }
            if let Some(notice) = crate::update::take_notice() {
                r.push_line(format!("! {notice}"), true);
                r.refresh()?;
            }
            continue;
        };
        match event {
            // A resize just needs a repaint; `refresh` re-reads the width.
            Event::Resize => r.refresh()?,
            Event::Key(k) => match edit_key(r, k) {
                KeyAction::Exit => return Ok(None),
                KeyAction::Submit(line) => {
                    r.push_line(format!("{PROMPT}{line}"), false);
                    return Ok(Some(line));
                }
                KeyAction::Edited => r.refresh()?,
            },
        }
    }
}

/// What a keystroke on the input line amounted to.
enum KeyAction {
    /// The editor changed (or the key was ignored); repaint.
    Edited,
    /// Enter: the line was taken out of the editor.
    Submit(String),
    /// Ctrl-D on an empty line.
    Exit,
}

/// Apply one key to the line editor. Shared by the idle prompt and the
/// working-state pump so editing feels identical in both.
fn edit_key(r: &mut Renderer, k: Key) -> KeyAction {
    match k {
        Key::Ctrl('d') if r.editor.is_empty() => return KeyAction::Exit,
        // Ctrl-C cancels the current (unsent) input.
        Key::Ctrl('c') => r.editor.clear(),
        Key::Char(c) => r.editor.insert(c),
        Key::Backspace => r.editor.backspace(),
        Key::Delete => r.editor.delete(),
        Key::Left => r.editor.left(),
        Key::Right => r.editor.right(),
        Key::Home => r.editor.home(),
        Key::End => r.editor.end(),
        Key::Enter => return KeyAction::Submit(r.editor.take()),
        _ => {}
    }
    KeyAction::Edited
}

/// Progress reported by the worker thread to the main thread, one variant per
/// [`Ui`] callback plus the terminal `Finished`.
enum UiEvent {
    Reasoning(String),
    Content(String),
    ToolStart {
        name: String,
        arguments: String,
    },
    ToolEnd {
        result: String,
        ok: bool,
    },
    TurnEnd,
    Info(String),
    /// A subagent's progress line.
    Activity(String),
    Notice(String),
    /// A note from the parallel reviewer ("subconscious").
    Subconscious(String),
    /// A queued message was appended to the conversation.
    Delivered(String),
    /// The worker is blocked waiting for an approval answer.
    AskApproval {
        tool: String,
        arguments: String,
    },
    /// The worker is blocked waiting for the user to answer a question.
    AskUser(Question),
    /// The turn is over; `Err` carries the rendered error, if any. Always the
    /// last event a worker sends.
    Finished(Result<(), String>),
}

/// Run one model turn on a scoped worker thread while the main thread keeps
/// the keyboard and the screen. Returns once the turn is over (or the worker
/// vanished); the renderer is left idle.
fn run_turn(session: &mut Session, r: &mut Renderer, input: &str) -> Result<()> {
    let (ev_tx, ev_rx) = mpsc::channel::<UiEvent>();
    let (ans_tx, ans_rx) = mpsc::channel::<agent::Approval>();
    let (reply_tx, reply_rx) = mpsc::channel::<Answer>();
    let queue = Arc::clone(&r.queue);
    let stop = Arc::clone(&r.stop);
    stop.store(false, Ordering::Relaxed);

    r.working = Some(Instant::now());
    r.approval_pending = false;
    r.question = None;
    r.refresh()?;

    let result = std::thread::scope(|s| {
        s.spawn(move || {
            let mut ui = ChanUi {
                tx: ev_tx.clone(),
                answers: ans_rx,
                replies: reply_rx,
                queue,
                stop,
            };
            let outcome = session.send(input, &mut ui).map_err(|e| format!("{e:#}"));
            // The receiver only disappears if the pump failed; nothing to do.
            let _ = ev_tx.send(UiEvent::Finished(outcome));
        });
        // If the pump errors out, the senders drop with it, which unblocks a
        // worker waiting on an approval (it sees `Deny`) or on an answer (no
        // one to ask) so the scope can join.
        pump(r, &ev_rx, ans_tx, reply_tx)
    });

    r.working = None;
    r.approval_pending = false;
    r.question = None;
    r.commit_pending_if_any();
    result
}

/// The working-state event loop: interleave keyboard input (edit/queue lines,
/// answer approvals and questions) with the worker's output until it reports
/// `Finished`. It owns the answer senders, so they are gone when it returns.
fn pump(
    r: &mut Renderer,
    events: &Receiver<UiEvent>,
    answers: Sender<agent::Approval>,
    replies: Sender<Answer>,
) -> Result<()> {
    let mut last_paint = Instant::now();
    loop {
        // Keyboard first; the poll timeout doubles as the loop's sleep.
        match term::poll(KEY_POLL)? {
            Some(Event::Resize) => r.refresh()?,
            Some(Event::Key(k)) => {
                if r.approval_pending {
                    if let Some(a) = approval_key(k) {
                        r.approval_pending = false;
                        let _ = answers.send(a);
                    }
                } else if r.question.is_some() {
                    if let Some(answer) = question_key(r, k) {
                        let _ = replies.send(answer);
                    }
                } else {
                    working_key(r, k);
                }
                r.refresh()?;
            }
            None => {}
        }

        // Then everything the worker produced meanwhile, painted once.
        let mut dirty = false;
        loop {
            match events.try_recv() {
                Ok(UiEvent::Finished(outcome)) => {
                    if let Err(e) = outcome {
                        r.push_line(format!("error: {e}"), false);
                    }
                    return Ok(());
                }
                Ok(ev) => {
                    apply_event(r, ev);
                    dirty = true;
                }
                Err(TryRecvError::Empty) => break,
                // The worker died without saying goodbye (a panic — which the
                // scope re-raises once we return).
                Err(TryRecvError::Disconnected) => return Ok(()),
            }
        }
        if dirty || last_paint.elapsed() >= SPINNER_TICK {
            r.refresh()?;
            last_paint = Instant::now();
        }
    }
}

/// A keystroke while a turn is running: edit the line as usual, but Enter
/// queues it rather than sending. Ctrl-C on an empty line drops the queue,
/// or, with nothing queued, stops the turn.
fn working_key(r: &mut Renderer, k: Key) {
    if k == Key::Ctrl('c') && r.editor.is_empty() {
        let dropped = r.clear_queue();
        if dropped > 0 {
            r.push_line(format!("! dropped {dropped} queued message(s)"), true);
        } else if !r.stop.swap(true, Ordering::Relaxed) {
            r.commit_pending_if_any();
            r.push_line("! stopping…".into(), true);
        }
        return;
    }
    match edit_key(r, k) {
        KeyAction::Submit(line) => {
            if !line.trim().is_empty() {
                r.enqueue(line);
            }
        }
        // Ctrl-D never exits mid-turn: the worker must finish first.
        KeyAction::Exit | KeyAction::Edited => {}
    }
}

/// A keystroke while a question from the model is pending: the input line
/// edits as usual, and Enter answers the question with it instead of queuing
/// it. `Some` once there is an answer to hand back. Ctrl-C keeps doing one
/// thing per press: it clears typed text first, then dismisses the question.
fn question_key(r: &mut Renderer, k: Key) -> Option<Answer> {
    if k == Key::Ctrl('c') && r.editor.is_empty() {
        r.question = None;
        r.push_line("! question dismissed".into(), true);
        return Some(Answer::Declined);
    }
    match edit_key(r, k) {
        KeyAction::Submit(line) => r.answer(&line),
        // Ctrl-D never exits while the worker waits on us.
        KeyAction::Exit | KeyAction::Edited => None,
    }
}

/// Map a key to an approval answer; `None` for keys that don't answer.
fn approval_key(k: Key) -> Option<agent::Approval> {
    match k {
        Key::Char('y' | 'Y') => Some(agent::Approval::Once),
        Key::Char('a' | 'A') => Some(agent::Approval::Always),
        Key::Char('n' | 'N') | Key::Esc | Key::Ctrl('c') => Some(agent::Approval::Deny),
        _ => None,
    }
}

/// Render one worker event into the scrollback / live region. Reasoning is
/// dimmed, the answer is normal, tool activity is annotated — mirroring
/// [`crate::agent::StdoutUi`].
fn apply_event(r: &mut Renderer, ev: UiEvent) {
    match ev {
        UiEvent::Reasoning(t) => r.emit(&t, true),
        UiEvent::Content(t) => r.emit(&t, false),
        UiEvent::ToolStart { name, arguments } => {
            let args = truncate_tail_head(&arguments, 200);
            r.emit_block(&format!("⚙ {name} {args}"), true);
        }
        UiEvent::ToolEnd { result, ok } => {
            let mark = if ok { "✓" } else { "✗" };
            let preview: String = result.chars().take(200).collect();
            r.emit_block(&format!("{mark} {preview}"), true);
        }
        UiEvent::TurnEnd => {
            r.commit_pending_if_any();
            // A blank line separates turns in the scrollback.
            r.push_line(String::new(), false);
        }
        UiEvent::Info(t) => r.emit_block(&t, false),
        UiEvent::Activity(t) => r.emit_block(&t, true),
        UiEvent::Notice(t) => r.emit_block(&format!("! {t}"), false),
        UiEvent::Subconscious(t) => r.emit_block(&format!("💭 {t}"), true),
        UiEvent::Delivered(t) => {
            // Record the message where the model actually received it.
            r.commit_pending_if_any();
            r.push_line(format!("{PROMPT}{t}"), false);
        }
        UiEvent::AskApproval { tool, arguments } => {
            let args = truncate_tail_head(&arguments, 200);
            r.emit_block(&format!("⚠ allow tool '{tool}'?  {args}"), false);
            r.emit_block("   [y] once   [a] always   [n] deny", false);
            r.approval_pending = true;
        }
        UiEvent::AskUser(question) => r.ask(question),
        UiEvent::Finished(_) => {} // consumed by the pump
    }
}

/// The worker-side [`Ui`]: forwards every callback to the main thread and
/// blocks only for approval answers and answers to questions. Queued input is read straight from the
/// shared queue.
struct ChanUi {
    tx: Sender<UiEvent>,
    answers: Receiver<agent::Approval>,
    replies: Receiver<Answer>,
    queue: Queue,
    stop: Arc<AtomicBool>,
}

impl ChanUi {
    fn send(&self, ev: UiEvent) {
        let _ = self.tx.send(ev);
    }
}

impl Ui for ChanUi {
    fn reasoning(&mut self, text: &str) {
        self.send(UiEvent::Reasoning(text.to_string()));
    }
    fn content(&mut self, text: &str) {
        self.send(UiEvent::Content(text.to_string()));
    }
    fn tool_start(&mut self, name: &str, arguments: &str) {
        self.send(UiEvent::ToolStart {
            name: name.to_string(),
            arguments: arguments.to_string(),
        });
    }
    fn tool_end(&mut self, _name: &str, result: &str, ok: bool) {
        self.send(UiEvent::ToolEnd {
            result: result.to_string(),
            ok,
        });
    }
    fn turn_end(&mut self) {
        self.send(UiEvent::TurnEnd);
    }
    fn info(&mut self, text: &str) {
        self.send(UiEvent::Info(text.to_string()));
    }
    fn activity(&mut self, text: &str) {
        self.send(UiEvent::Activity(text.to_string()));
    }
    fn ask_approval(&mut self, tool: &str, arguments: &str) -> agent::Approval {
        self.send(UiEvent::AskApproval {
            tool: tool.to_string(),
            arguments: arguments.to_string(),
        });
        // A vanished main thread means no one can approve: deny.
        self.answers.recv().unwrap_or(agent::Approval::Deny)
    }
    fn ask_user(&mut self, question: &Question) -> Option<Answer> {
        self.send(UiEvent::AskUser(question.clone()));
        // A vanished main thread means there is no one to ask.
        self.replies.recv().ok()
    }
    fn notice(&mut self, text: &str) {
        self.send(UiEvent::Notice(text.to_string()));
    }
    fn subconscious(&mut self, text: &str) {
        self.send(UiEvent::Subconscious(text.to_string()));
    }
    fn take_queued(&mut self) -> Vec<String> {
        take_leading_prompts(&mut lock(&self.queue))
    }
    fn queued_delivered(&mut self, text: &str) {
        self.send(UiEvent::Delivered(text.to_string()));
    }
    fn interrupted(&mut self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }
}

/// Pop queued lines from the front up to (not including) the first slash
/// command. Prompts are delivered to the running turn; commands, and anything
/// queued behind them, wait for the turn to end so they run in order.
fn take_leading_prompts(q: &mut VecDeque<String>) -> Vec<String> {
    let mut out = Vec::new();
    while q.front().is_some_and(|l| !l.trim_start().starts_with('/')) {
        out.extend(q.pop_front());
    }
    out
}

/// Lock the queue, tolerating a poisoned mutex (the data is plain strings).
fn lock(q: &Queue) -> std::sync::MutexGuard<'_, VecDeque<String>> {
    q.lock().unwrap_or_else(|p| p.into_inner())
}

/// One committed line of scrollback, remembered until the next [`Renderer::refresh`]
/// prints (and thereby immortalises) it.
struct ScrollLine {
    text: String,
    dim: bool,
}

/// Owns the live region and the append-only commit queue.
struct Renderer {
    editor: LineEditor,
    /// The status strip contents (recomputed per turn).
    status: String,
    /// Lines awaiting their one-and-only print into scrollback.
    scroll: Vec<ScrollLine>,
    /// The in-progress (not yet newline-terminated) output line.
    pending: String,
    /// Whether `pending` is styled as reasoning (dimmed).
    pending_dim: bool,
    /// Row count of the live region as last drawn, so the next refresh knows
    /// how far up to walk before clearing.
    live_height: u16,
    /// When the running turn started; `None` while idle. Drives the spinner
    /// and elapsed counter in the status strip.
    working: Option<Instant>,
    /// The worker is blocked on an approval prompt: keys answer it instead of
    /// editing the line.
    approval_pending: bool,
    /// A question from the model (the `ask` tool) the user hasn't answered
    /// yet: the next submitted line answers it.
    question: Option<Question>,
    /// Lines submitted while working, awaiting delivery.
    queue: Queue,
    /// Set by Ctrl-C to stop the running turn; read by the worker through
    /// [`Ui::interrupted`], cleared when a turn starts.
    stop: Arc<AtomicBool>,
}

impl Renderer {
    fn new() -> Self {
        Self {
            editor: LineEditor::new(),
            status: String::new(),
            scroll: Vec::new(),
            pending: String::new(),
            pending_dim: false,
            live_height: 0,
            working: None,
            approval_pending: false,
            question: None,
            queue: Arc::new(Mutex::new(VecDeque::new())),
            stop: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Queue a line typed while a turn runs, echoing it (dimmed, marked) so
    /// the user sees it was accepted. It is echoed again, undimmed, when it is
    /// actually delivered.
    fn enqueue(&mut self, line: String) {
        self.push_line(format!("{PROMPT}{line}  (queued)"), true);
        lock(&self.queue).push_back(line);
    }

    /// Print a question from the model — itself, its numbered options and
    /// how to answer, as ordinary scrollback lines — and wait for the answer
    /// on the input line.
    fn ask(&mut self, question: Question) {
        self.commit_pending_if_any();
        for line in question.lines() {
            self.push_line(line, false);
        }
        self.question = Some(question);
    }

    /// Take a submitted line as the answer to the pending question. The line
    /// is echoed like any input; if it can't be understood, the reason and
    /// the hint are printed and the question stays pending (`None`).
    fn answer(&mut self, line: &str) -> Option<Answer> {
        let question = self.question.take()?;
        self.push_line(format!("{PROMPT}{line}"), false);
        match question.parse(line) {
            Ok(answer) => Some(answer),
            Err(why) => {
                self.push_line(format!("  {why}"), true);
                self.push_line(question.hint(), false);
                self.question = Some(question);
                None
            }
        }
    }

    /// The next queued line, if any, in submission order.
    fn pop_queued(&mut self) -> Option<String> {
        lock(&self.queue).pop_front()
    }

    fn queued_len(&self) -> usize {
        lock(&self.queue).len()
    }

    /// Drop everything queued; returns how many lines were discarded.
    fn clear_queue(&mut self) -> usize {
        let mut q = lock(&self.queue);
        let n = q.len();
        q.clear();
        n
    }

    /// The status strip as shown: the working indicator (spinner, elapsed,
    /// pending approval or question) and queue depth lead, so they survive clipping on a
    /// narrow terminal; then the per-turn `status` (model · cwd · …).
    fn status_line(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if self.approval_pending {
            parts.push("⚠ approval needed [y/a/n]".into());
        } else if self.question.is_some() {
            parts.push("? waiting for your answer".into());
        } else if let Some(t0) = self.working {
            let elapsed = t0.elapsed();
            parts.push(format!(
                "{} working {}s",
                spinner_frame(elapsed),
                elapsed.as_secs()
            ));
        }
        let queued = self.queued_len();
        if queued > 0 {
            parts.push(format!("{queued} queued"));
        }
        if parts.is_empty() {
            self.status.clone()
        } else {
            // `status` carries its own leading space.
            format!(" {} ·{}", parts.join(" · "), self.status)
        }
    }

    /// Queue a whole line for the scrollback.
    fn push_line(&mut self, text: String, dim: bool) {
        self.scroll.push(ScrollLine { text, dim });
    }

    /// Commit the current partial line (even if empty — preserves blank lines).
    fn flush_line(&mut self) {
        let text = std::mem::take(&mut self.pending);
        let dim = self.pending_dim;
        self.push_line(text, dim);
    }

    /// Commit the current partial line only if it holds anything.
    fn commit_pending_if_any(&mut self) {
        if !self.pending.is_empty() {
            self.flush_line();
        }
    }

    /// Feed streamed text, splitting on newlines. Complete lines are committed
    /// to scrollback; the trailing fragment stays `pending` and is shown live.
    fn emit(&mut self, text: &str, dim: bool) {
        // A style change mid-stream forces the current fragment to commit so a
        // single scrollback line never mixes reasoning and answer styling.
        if dim != self.pending_dim && !self.pending.is_empty() {
            self.commit_pending_if_any();
        }
        self.pending_dim = dim;
        for ch in text.chars() {
            match ch {
                '\n' => self.flush_line(),
                '\r' => {} // ignore bare carriage returns from the stream
                _ => self.pending.push(ch),
            }
        }
    }

    /// Emit text that should be committed in full immediately (tool activity,
    /// notices) rather than left dangling as a live fragment.
    fn emit_block(&mut self, text: &str, dim: bool) {
        self.emit(text, dim);
        self.commit_pending_if_any();
    }

    /// Repaint the live region, first flushing any committed lines into the
    /// scrollback above it. This is the sole routine that upholds the
    /// append-only invariant; see the module docs.
    fn refresh(&mut self) -> io::Result<()> {
        let width = term::size().map_or(80, |(w, _)| w).max(1);
        let mut out = io::stdout().lock();

        // Walk to the top-left of the previously drawn live region and wipe
        // it (nothing above it is ever touched).
        write!(
            out,
            "{}{}{}{}",
            term::HIDE_CURSOR,
            term::move_up(self.live_height.saturating_sub(1)),
            term::move_to_column(0),
            term::CLEAR_DOWN
        )?;

        // Flush committed lines — printed once here, then immutable scrollback.
        for line in std::mem::take(&mut self.scroll) {
            let style = if line.dim { term::DIM } else { "" };
            write!(out, "{style}{}{}\r\n", line.text, term::RESET)?;
        }

        // --- Draw the live region (top to bottom) ---
        let mut height: u16 = 0;

        // 1. The in-progress output fragment, if any (dim when reasoning).
        if !self.pending.is_empty() {
            let shown = truncate_tail(&self.pending, width as usize);
            let style = if self.pending_dim { term::DIM } else { "" };
            write!(out, "{style}{shown}{}\r\n", term::RESET)?;
            height += 1;
        }

        // 2. The status strip: a reverse-video bar padded to full width.
        let bar = pad_to(&self.status_line(), width as usize);
        write!(out, "{}{bar}{}\r\n", term::REVERSE, term::RESET)?;
        height += 1;

        // 3. The input line, horizontally scrolled to keep the cursor visible.
        let (shown, col) = self.editor.view(width as usize, PROMPT_COLS);
        write!(out, "{PROMPT}{shown}")?;
        height += 1;

        // Park the cursor at the edit position on the input row.
        write!(
            out,
            "{}{}",
            term::move_to_column(col as u16),
            term::SHOW_CURSOR
        )?;
        self.live_height = height;
        out.flush()
    }
}

/// A synchronous [`Ui`] over the renderer, used on the main thread for slash
/// commands (`/help`, `/mcp`, …) whose output is printed in place. Model turns
/// go through [`ChanUi`] instead, so the styling here mirrors [`apply_event`].
struct TuiUi<'a> {
    r: &'a mut Renderer,
}

impl Ui for TuiUi<'_> {
    fn reasoning(&mut self, text: &str) {
        self.r.emit(text, true);
        let _ = self.r.refresh();
    }
    fn content(&mut self, text: &str) {
        self.r.emit(text, false);
        let _ = self.r.refresh();
    }
    fn tool_start(&mut self, name: &str, arguments: &str) {
        let args = truncate_tail_head(arguments, 200);
        self.r.emit_block(&format!("⚙ {name} {args}"), true);
        let _ = self.r.refresh();
    }
    fn tool_end(&mut self, _name: &str, result: &str, ok: bool) {
        let mark = if ok { "✓" } else { "✗" };
        let preview: String = result.chars().take(200).collect();
        self.r.emit_block(&format!("{mark} {preview}"), true);
        let _ = self.r.refresh();
    }
    fn turn_end(&mut self) {
        self.r.commit_pending_if_any();
        // A blank line separates turns in the scrollback.
        self.r.push_line(String::new(), false);
        let _ = self.r.refresh();
    }
    fn info(&mut self, text: &str) {
        self.r.emit_block(text, false);
        let _ = self.r.refresh();
    }
    fn activity(&mut self, text: &str) {
        self.r.emit_block(text, true);
        let _ = self.r.refresh();
    }
    fn ask_approval(&mut self, tool: &str, arguments: &str) -> agent::Approval {
        let args = truncate_tail_head(arguments, 200);
        self.r
            .emit_block(&format!("⚠ allow tool '{tool}'?  {args}"), false);
        self.r
            .emit_block("   [y] once   [a] always   [n] deny", false);
        let _ = self.r.refresh();
        loop {
            match term::read() {
                Ok(Event::Key(k)) => {
                    if let Some(answer) = approval_key(k) {
                        return answer;
                    }
                }
                Ok(Event::Resize) => {}
                Err(_) => return agent::Approval::Deny,
            }
        }
    }
    /// Reached when claude's background work asks between turns: there is
    /// no pump then, so the keyboard is read here until the answer is in.
    fn ask_user(&mut self, question: &Question) -> Option<Answer> {
        self.r.ask(question.clone());
        loop {
            let _ = self.r.refresh();
            match term::read() {
                Ok(Event::Key(k)) => {
                    if let Some(answer) = question_key(self.r, k) {
                        let _ = self.r.refresh();
                        return Some(answer);
                    }
                }
                Ok(Event::Resize) => {}
                Err(_) => {
                    self.r.question = None;
                    return Some(Answer::Declined);
                }
            }
        }
    }
    fn notice(&mut self, text: &str) {
        self.r.emit_block(&format!("! {text}"), false);
        let _ = self.r.refresh();
    }
    fn subconscious(&mut self, text: &str) {
        self.r.emit_block(&format!("💭 {text}"), true);
        let _ = self.r.refresh();
    }
}

/// Build the status strip: `model · cwd · branch · turn N`, then the
/// permission mode when it is not the default one.
fn build_status(session: &Session, turn: u32) -> String {
    let model = &session.config().model;
    let cwd = std::env::current_dir()
        .ok()
        .and_then(|p| p.file_name().map(|s| s.to_string_lossy().into_owned()))
        .unwrap_or_else(|| "?".into());
    let mut s = format!(" {model} · {cwd}");
    if let Some(branch) = git_branch() {
        s.push_str(" · ");
        s.push_str(&branch);
    }
    s.push_str(&format!(" · turn {turn}"));
    let mode = session.mode();
    if mode != Mode::default() {
        s.push_str(&format!(" · {mode}"));
    }
    if let Some(u) = session.usage_summary() {
        s.push_str(" · ");
        s.push_str(&u);
    }
    s
}

/// Best-effort current git branch (`git branch --show-current`).
fn git_branch() -> Option<String> {
    let out = std::process::Command::new("git")
        .args(["branch", "--show-current"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let branch = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if branch.is_empty() {
        None
    } else {
        Some(branch)
    }
}

/// The spinner glyph for a given time since the turn started.
fn spinner_frame(elapsed: Duration) -> char {
    let idx = (elapsed.as_millis() / SPINNER_TICK.as_millis()) as usize % SPINNER.len();
    SPINNER[idx]
}

/// Truncate `s` to `width` columns, keeping the tail (with a leading `…` when
/// clipped). Used for the live output fragment so the freshest text is visible.
fn truncate_tail(s: &str, width: usize) -> String {
    let count = s.chars().count();
    if count <= width {
        return s.to_string();
    }
    if width <= 1 {
        return s.chars().skip(count - width).collect();
    }
    let tail: String = s.chars().skip(count - (width - 1)).collect();
    format!("…{tail}")
}

/// Truncate `s` to at most `max` chars, keeping the head (with a trailing `…`).
fn truncate_tail_head(s: &str, max: usize) -> String {
    let count = s.chars().count();
    if count <= max {
        return s.to_string();
    }
    let head: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{head}…")
}

/// Clip `s` to `width` columns, right-padding with spaces to exactly fill it.
fn pad_to(s: &str, width: usize) -> String {
    let mut out: String = s.chars().take(width).collect();
    let len = out.chars().count();
    if len < width {
        out.push_str(&" ".repeat(width - len));
    }
    out
}

/// Enables raw mode and restores the terminal on drop — on the normal path, on a
/// `?` early return, and while unwinding from a panic.
struct RawGuard {
    _raw: term::RawMode,
}

impl RawGuard {
    fn enable() -> Result<Self> {
        Ok(RawGuard {
            _raw: term::RawMode::enable()?,
        })
    }
}

impl Drop for RawGuard {
    fn drop(&mut self) {
        // Still raw here (the modes come back when `_raw` drops, after
        // this): leave the shell prompt on a fresh line below the last
        // input row.
        let mut out = io::stdout();
        let _ = write!(out, "{}{}\r\n", term::RESET, term::SHOW_CURSOR);
        let _ = out.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pad_to_fills_and_clips() {
        assert_eq!(pad_to("ab", 5), "ab   ");
        assert_eq!(pad_to("abcdef", 3), "abc");
        assert_eq!(pad_to("", 2), "  ");
        assert_eq!(pad_to("x", 0), "");
    }

    #[test]
    fn truncate_tail_keeps_end() {
        assert_eq!(truncate_tail("abc", 10), "abc");
        assert_eq!(truncate_tail("abcdef", 4), "…def");
        // Degenerate widths must not panic.
        assert_eq!(truncate_tail("abc", 1), "c");
        assert_eq!(truncate_tail("abc", 0), "");
    }

    #[test]
    fn truncate_head_keeps_start() {
        assert_eq!(truncate_tail_head("abc", 10), "abc");
        assert_eq!(truncate_tail_head("abcdef", 4), "abc…");
    }

    #[test]
    fn emit_commits_lines_and_keeps_partial() {
        let mut r = Renderer::new();
        r.emit("hello\nwor", false);
        // "hello" committed; "wor" still pending.
        assert_eq!(r.scroll.len(), 1);
        assert_eq!(r.scroll[0].text, "hello");
        assert_eq!(r.pending, "wor");
        r.emit("ld\n", false);
        assert_eq!(r.scroll.len(), 2);
        assert_eq!(r.scroll[1].text, "world");
        assert!(r.pending.is_empty());
    }

    #[test]
    fn emit_preserves_blank_lines() {
        let mut r = Renderer::new();
        r.emit("a\n\nb\n", false);
        let texts: Vec<&str> = r.scroll.iter().map(|l| l.text.as_str()).collect();
        assert_eq!(texts, vec!["a", "", "b"]);
    }

    #[test]
    fn style_switch_commits_pending_fragment() {
        let mut r = Renderer::new();
        r.emit("thinking", true); // reasoning, no newline -> pending
        assert!(r.scroll.is_empty());
        r.emit("answer", false); // switching to content commits the fragment
        assert_eq!(r.scroll.len(), 1);
        assert_eq!(r.scroll[0].text, "thinking");
        assert!(r.scroll[0].dim);
        assert_eq!(r.pending, "answer");
        assert!(!r.pending_dim);
    }

    #[test]
    fn take_leading_prompts_stops_at_commands() {
        let mut q: VecDeque<String> = ["a", "b", "/help", "c"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(take_leading_prompts(&mut q), vec!["a", "b"]);
        // The command and everything behind it wait for the turn to end.
        assert_eq!(q, ["/help", "c"]);
        assert!(take_leading_prompts(&mut q).is_empty());
        assert_eq!(q.len(), 2);
    }

    #[test]
    fn enqueue_echoes_and_status_counts() {
        let mut r = Renderer::new();
        r.status = " m · d".into();
        assert_eq!(r.status_line(), " m · d");
        r.enqueue("later".into());
        assert_eq!(r.scroll.len(), 1);
        assert!(r.scroll[0].dim);
        assert!(r.scroll[0].text.ends_with("(queued)"));
        r.working = Some(Instant::now());
        let line = r.status_line();
        assert!(line.contains("working"), "{line}");
        assert!(line.contains("1 queued"), "{line}");
        assert!(line.ends_with(" m · d"), "{line}");
        assert_eq!(r.pop_queued().as_deref(), Some("later"));
        assert_eq!(r.pop_queued(), None);
        assert_eq!(r.clear_queue(), 0);
    }

    fn typed(r: &mut Renderer, text: &str) -> Option<Answer> {
        for c in text.chars() {
            assert_eq!(question_key(r, Key::Char(c)), None);
        }
        question_key(r, Key::Enter)
    }

    fn sample_question(allow_other: bool) -> Question {
        Question::from_args(&serde_json::json!({
            "question": "Which database?",
            "options": [
                {"label": "Postgres", "description": "needs a server"},
                {"label": "SQLite"},
                {"label": "Plain files"},
            ],
            "allow_other": allow_other,
        }))
        .unwrap()
    }

    fn texts(r: &Renderer) -> Vec<&str> {
        r.scroll.iter().map(|l| l.text.as_str()).collect()
    }

    #[test]
    fn a_question_is_printed_and_answered_on_the_input_line() {
        let mut r = Renderer::new();
        r.working = Some(Instant::now());
        r.emit("partial", false);
        apply_event(&mut r, UiEvent::AskUser(sample_question(true)));
        // Ordinary scrollback lines, after whatever was mid-line.
        assert_eq!(
            texts(&r),
            vec![
                "partial",
                "? Which database?",
                "  1. Postgres — needs a server",
                "  2. SQLite",
                "  3. Plain files",
                "  (answer with a number, or type your own answer)",
            ]
        );
        let status = r.status_line();
        assert!(status.contains("? waiting for your answer"), "{status}");
        assert!(!status.contains("working"), "{status}");

        // The submitted line answers the question: echoed, not queued.
        r.scroll.clear();
        assert_eq!(
            typed(&mut r, "2"),
            Some(Answer::Labels(vec!["SQLite".into()]))
        );
        assert_eq!(texts(&r), vec!["› 2"]);
        assert_eq!(r.queued_len(), 0);
        assert!(r.question.is_none());
        assert!(r.editor.is_empty());
        assert!(r.status_line().contains("working"));
    }

    #[test]
    fn an_answer_that_cannot_be_understood_reprints_the_hint_and_waits() {
        let mut r = Renderer::new();
        r.ask(sample_question(false));
        r.scroll.clear();
        assert_eq!(typed(&mut r, "9"), None);
        assert_eq!(
            texts(&r),
            vec![
                "› 9",
                "  \"9\" is not one of the 3 options",
                "  (answer with a number)",
            ]
        );
        assert!(r.question.is_some());
        assert!(r.status_line().contains("waiting for your answer"));
        // An empty line is no answer either.
        assert_eq!(typed(&mut r, ""), None);
        assert!(r.question.is_some());
        // Then a good one goes through.
        assert_eq!(
            typed(&mut r, "plain"),
            Some(Answer::Labels(vec!["Plain files".into()]))
        );
        assert!(r.question.is_none());
    }

    #[test]
    fn a_typed_answer_of_the_users_own_is_passed_on_where_allowed() {
        let mut r = Renderer::new();
        r.ask(sample_question(true));
        assert_eq!(typed(&mut r, "MySQL"), Some(Answer::Text("MySQL".into())));
    }

    #[test]
    fn ctrl_c_clears_the_line_then_dismisses_the_question() {
        let mut r = Renderer::new();
        r.ask(sample_question(true));
        assert_eq!(question_key(&mut r, Key::Char('2')), None);
        // First press: only the typed text goes.
        assert_eq!(question_key(&mut r, Key::Ctrl('c')), None);
        assert!(r.editor.is_empty());
        assert!(r.question.is_some());
        // Second: the question; the turn itself is not stopped.
        assert_eq!(question_key(&mut r, Key::Ctrl('c')), Some(Answer::Declined));
        assert!(r.question.is_none());
        assert!(!r.stop.load(Ordering::Relaxed));
        assert_eq!(r.scroll.last().unwrap().text, "! question dismissed");
        // Ctrl-D doesn't exit or answer while a question waits.
        r.ask(sample_question(true));
        assert_eq!(question_key(&mut r, Key::Ctrl('d')), None);
        assert!(r.question.is_some());
    }

    #[test]
    fn spinner_cycles() {
        assert_eq!(spinner_frame(Duration::ZERO), SPINNER[0]);
        assert_eq!(spinner_frame(SPINNER_TICK), SPINNER[1]);
        let full = SPINNER_TICK * SPINNER.len() as u32;
        assert_eq!(spinner_frame(full), SPINNER[0]);
    }

    #[test]
    fn emit_block_commits_fully() {
        let mut r = Renderer::new();
        r.emit_block("⚙ read {}", true);
        assert_eq!(r.scroll.len(), 1);
        assert!(r.pending.is_empty());
    }
}
