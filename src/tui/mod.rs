//! Minimal terminal interface.
//!
//! The intended interface (roadmap M2): a single input line plus a status strip
//! (model · cwd · git branch · token counters). The input row redraws in place;
//! **everything else — assistant text, reasoning, tool activity — prints to the
//! terminal scrollback and is never redrawn.** No panes, no mouse, no
//! alt-screen. Streamed output must interleave above the live input line
//! without corrupting it. `crossterm` (already a dependency) drives raw mode.
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
//! - **Ctrl-C cannot cancel an in-flight turn.** The [`Ui`] trait exposes no
//!   cancellation channel, so a running turn always completes. Ctrl-C clears
//!   the current input; while a turn runs, Ctrl-C on an empty input drops the
//!   queued messages instead. Ctrl-D on an empty line exits between turns.
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

mod input;

use std::collections::VecDeque;
use std::io::{self, IsTerminal, Write};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::style::{Attribute, Print, SetAttribute};
use crossterm::terminal::{Clear, ClearType};
use crossterm::{cursor, event, execute, queue, terminal};

use crate::agent::{self, Session, Ui};

use input::LineEditor;

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
            None => match read_line(&mut r)? {
                Some(line) => line,
                None => break, // Ctrl-D
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
                r.refresh()?;
            }
            agent::Dispatch::Prompt => {
                turn += 1;
                r.status = build_status(&session, turn);
                run_turn(&mut session, &mut r, &trimmed)?;
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
/// into the scrollback record.
fn read_line(r: &mut Renderer) -> Result<Option<String>> {
    loop {
        match event::read()? {
            // A resize just needs a repaint; `refresh` re-reads the width.
            Event::Resize(_, _) => r.refresh()?,
            Event::Key(k) if k.kind != KeyEventKind::Release => match edit_key(r, k) {
                KeyAction::Exit => return Ok(None),
                KeyAction::Submit(line) => {
                    r.push_line(format!("{PROMPT}{line}"), false);
                    return Ok(Some(line));
                }
                KeyAction::Edited => r.refresh()?,
            },
            _ => {}
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
fn edit_key(r: &mut Renderer, k: KeyEvent) -> KeyAction {
    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
    match k.code {
        KeyCode::Char('d') if ctrl && r.editor.is_empty() => return KeyAction::Exit,
        // Ctrl-C cancels the current (unsent) input.
        KeyCode::Char('c') if ctrl => r.editor.clear(),
        KeyCode::Char(c) if !ctrl => r.editor.insert(c),
        KeyCode::Backspace => r.editor.backspace(),
        KeyCode::Delete => r.editor.delete(),
        KeyCode::Left => r.editor.left(),
        KeyCode::Right => r.editor.right(),
        KeyCode::Home => r.editor.home(),
        KeyCode::End => r.editor.end(),
        KeyCode::Enter => return KeyAction::Submit(r.editor.take()),
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
    Notice(String),
    /// A queued message was appended to the conversation.
    Delivered(String),
    /// The worker is blocked waiting for an approval answer.
    AskApproval {
        tool: String,
        arguments: String,
    },
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
    let queue = Arc::clone(&r.queue);

    r.working = Some(Instant::now());
    r.approval_pending = false;
    r.refresh()?;

    let result = std::thread::scope(|s| {
        s.spawn(move || {
            let mut ui = ChanUi {
                tx: ev_tx.clone(),
                answers: ans_rx,
                queue,
            };
            let outcome = session.send(input, &mut ui).map_err(|e| format!("{e:#}"));
            // The receiver only disappears if the pump failed; nothing to do.
            let _ = ev_tx.send(UiEvent::Finished(outcome));
        });
        // If the pump errors out, `ans_tx` drops with it, which unblocks a
        // worker waiting on an approval (it sees `Deny`) so the scope can join.
        pump(r, &ev_rx, &ans_tx)
    });

    r.working = None;
    r.approval_pending = false;
    r.commit_pending_if_any();
    result
}

/// The working-state event loop: interleave keyboard input (edit/queue lines,
/// answer approvals) with the worker's output until it reports `Finished`.
fn pump(
    r: &mut Renderer,
    events: &Receiver<UiEvent>,
    answers: &Sender<agent::Approval>,
) -> Result<()> {
    let mut last_paint = Instant::now();
    loop {
        // Keyboard first; the poll timeout doubles as the loop's sleep.
        if event::poll(KEY_POLL)? {
            match event::read()? {
                Event::Resize(_, _) => r.refresh()?,
                Event::Key(k) if k.kind != KeyEventKind::Release => {
                    if r.approval_pending {
                        if let Some(a) = approval_key(k) {
                            r.approval_pending = false;
                            let _ = answers.send(a);
                        }
                    } else {
                        working_key(r, k);
                    }
                    r.refresh()?;
                }
                _ => {}
            }
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
/// queues it rather than sending, and Ctrl-C on an empty line drops the queue.
fn working_key(r: &mut Renderer, k: KeyEvent) {
    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
    if ctrl && k.code == KeyCode::Char('c') && r.editor.is_empty() {
        let dropped = r.clear_queue();
        if dropped > 0 {
            r.push_line(format!("! dropped {dropped} queued message(s)"), true);
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

/// Map a key to an approval answer; `None` for keys that don't answer.
fn approval_key(k: KeyEvent) -> Option<agent::Approval> {
    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
    match k.code {
        KeyCode::Char('y') | KeyCode::Char('Y') => Some(agent::Approval::Once),
        KeyCode::Char('a') | KeyCode::Char('A') => Some(agent::Approval::Always),
        KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => Some(agent::Approval::Deny),
        KeyCode::Char('c') if ctrl => Some(agent::Approval::Deny),
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
        UiEvent::Notice(t) => r.emit_block(&format!("! {t}"), false),
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
        UiEvent::Finished(_) => {} // consumed by the pump
    }
}

/// The worker-side [`Ui`]: forwards every callback to the main thread and
/// blocks only for approval answers. Queued input is read straight from the
/// shared queue.
struct ChanUi {
    tx: Sender<UiEvent>,
    answers: Receiver<agent::Approval>,
    queue: Queue,
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
    fn ask_approval(&mut self, tool: &str, arguments: &str) -> agent::Approval {
        self.send(UiEvent::AskApproval {
            tool: tool.to_string(),
            arguments: arguments.to_string(),
        });
        // A vanished main thread means no one can approve: deny.
        self.answers.recv().unwrap_or(agent::Approval::Deny)
    }
    fn notice(&mut self, text: &str) {
        self.send(UiEvent::Notice(text.to_string()));
    }
    fn take_queued(&mut self) -> Vec<String> {
        take_leading_prompts(&mut lock(&self.queue))
    }
    fn queued_delivered(&mut self, text: &str) {
        self.send(UiEvent::Delivered(text.to_string()));
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
    /// Lines submitted while working, awaiting delivery.
    queue: Queue,
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
            queue: Arc::new(Mutex::new(VecDeque::new())),
        }
    }

    /// Queue a line typed while a turn runs, echoing it (dimmed, marked) so
    /// the user sees it was accepted. It is echoed again, undimmed, when it is
    /// actually delivered.
    fn enqueue(&mut self, line: String) {
        self.push_line(format!("{PROMPT}{line}  (queued)"), true);
        lock(&self.queue).push_back(line);
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
    /// pending approval) and queue depth lead, so they survive clipping on a
    /// narrow terminal; then the per-turn `status` (model · cwd · …).
    fn status_line(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if self.approval_pending {
            parts.push("⚠ approval needed [y/a/n]".into());
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
        let width = terminal::size().map(|(w, _)| w).unwrap_or(80).max(1);
        let mut out = io::stdout();

        queue!(out, cursor::Hide)?;
        // Walk to the top-left of the previously drawn live region.
        if self.live_height > 1 {
            queue!(out, cursor::MoveUp(self.live_height - 1))?;
        }
        queue!(out, cursor::MoveToColumn(0))?;
        // Wipe the old live region (nothing above it is ever touched).
        queue!(out, Clear(ClearType::FromCursorDown))?;

        // Flush committed lines — printed once here, then immutable scrollback.
        for line in std::mem::take(&mut self.scroll) {
            if line.dim {
                queue!(out, SetAttribute(Attribute::Dim))?;
            }
            queue!(
                out,
                Print(&line.text),
                SetAttribute(Attribute::Reset),
                Print("\r\n")
            )?;
        }

        // --- Draw the live region (top to bottom) ---
        let mut height: u16 = 0;

        // 1. The in-progress output fragment, if any (dim when reasoning).
        if !self.pending.is_empty() {
            let shown = truncate_tail(&self.pending, width as usize);
            if self.pending_dim {
                queue!(out, SetAttribute(Attribute::Dim))?;
            }
            queue!(
                out,
                Print(shown),
                SetAttribute(Attribute::Reset),
                Print("\r\n")
            )?;
            height += 1;
        }

        // 2. The status strip: a reverse-video bar padded to full width.
        let bar = pad_to(&self.status_line(), width as usize);
        queue!(
            out,
            SetAttribute(Attribute::Reverse),
            Print(bar),
            SetAttribute(Attribute::Reset),
            Print("\r\n")
        )?;
        height += 1;

        // 3. The input line, horizontally scrolled to keep the cursor visible.
        let (shown, col) = self.editor.view(width as usize, PROMPT_COLS);
        queue!(out, Print(PROMPT), Print(shown))?;
        height += 1;

        // Park the cursor at the edit position on the input row.
        queue!(out, cursor::MoveToColumn(col as u16), cursor::Show)?;
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
    fn ask_approval(&mut self, tool: &str, arguments: &str) -> agent::Approval {
        let args = truncate_tail_head(arguments, 200);
        self.r
            .emit_block(&format!("⚠ allow tool '{tool}'?  {args}"), false);
        self.r
            .emit_block("   [y] once   [a] always   [n] deny", false);
        let _ = self.r.refresh();
        loop {
            match event::read() {
                Ok(Event::Key(k)) if k.kind != KeyEventKind::Release => {
                    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
                    match k.code {
                        KeyCode::Char('y') | KeyCode::Char('Y') => return agent::Approval::Once,
                        KeyCode::Char('a') | KeyCode::Char('A') => return agent::Approval::Always,
                        KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                            return agent::Approval::Deny;
                        }
                        KeyCode::Char('c') if ctrl => return agent::Approval::Deny,
                        _ => {}
                    }
                }
                Ok(_) => {}
                Err(_) => return agent::Approval::Deny,
            }
        }
    }
    fn notice(&mut self, text: &str) {
        self.r.emit_block(&format!("! {text}"), false);
        let _ = self.r.refresh();
    }
}

/// Build the status strip: `model · cwd · branch · turn N`.
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
struct RawGuard;

impl RawGuard {
    fn enable() -> Result<Self> {
        terminal::enable_raw_mode()?;
        Ok(RawGuard)
    }
}

impl Drop for RawGuard {
    fn drop(&mut self) {
        let mut out = io::stdout();
        let _ = execute!(out, SetAttribute(Attribute::Reset), cursor::Show);
        let _ = terminal::disable_raw_mode();
        // Leave the shell prompt on a fresh line below the last input row.
        let _ = write!(out, "\r\n");
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
