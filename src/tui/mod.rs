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
//! - **Ctrl-C cannot cancel an in-flight turn.** [`Session::send`] is a blocking,
//!   single-threaded call and the frozen [`Ui`] trait exposes no cancellation
//!   channel, so while a turn streams there is no thread reading the keyboard.
//!   Ctrl-C is handled only at the prompt (it clears the current input). Bytes
//!   typed mid-turn are buffered by the terminal and simply ignored — they never
//!   corrupt the display. Ctrl-D on an empty line exits between turns.
//! - **Terminal height must be ≥ 3 rows.** The live region occupies up to three
//!   rows and the redraw moves the cursor up by up to two; on a 1–2 row terminal
//!   the accounting degrades (no crash, but the strip may be clipped).
//! - Column math is one-per-`char` (no Unicode width); wide/combining glyphs
//!   mis-place the cursor. Mid-turn resizes are absorbed because [`Renderer::refresh`]
//!   re-reads the terminal width every time rather than caching it.

mod input;

use std::io::{self, IsTerminal, Write};

use anyhow::Result;
use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::style::{Attribute, Print, SetAttribute};
use crossterm::terminal::{Clear, ClearType};
use crossterm::{cursor, event, execute, queue, terminal};

use crate::agent::{self, Session, Ui};

use input::LineEditor;

/// The input prompt and its width in columns (`›` + space, both single-width).
const PROMPT: &str = "› ";
const PROMPT_COLS: usize = 2;

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
        match event::read()? {
            // A resize just needs a repaint; `refresh` re-reads the width.
            Event::Resize(_, _) => r.refresh()?,
            Event::Key(k) => {
                // Ignore key-release events (some platforms emit them).
                if k.kind == KeyEventKind::Release {
                    continue;
                }
                let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
                match k.code {
                    // Ctrl-D on an empty line exits.
                    KeyCode::Char('d') if ctrl && r.editor.is_empty() => break,
                    // Ctrl-C cancels the current (unsent) input.
                    KeyCode::Char('c') if ctrl => {
                        r.editor.clear();
                        r.refresh()?;
                    }
                    KeyCode::Char(c) if !ctrl => {
                        r.editor.insert(c);
                        r.refresh()?;
                    }
                    KeyCode::Backspace => {
                        r.editor.backspace();
                        r.refresh()?;
                    }
                    KeyCode::Delete => {
                        r.editor.delete();
                        r.refresh()?;
                    }
                    KeyCode::Left => {
                        r.editor.left();
                        r.refresh()?;
                    }
                    KeyCode::Right => {
                        r.editor.right();
                        r.refresh()?;
                    }
                    KeyCode::Home => {
                        r.editor.home();
                        r.refresh()?;
                    }
                    KeyCode::End => {
                        r.editor.end();
                        r.refresh()?;
                    }
                    KeyCode::Enter => {
                        let input = r.editor.take();
                        // Echo the submitted prompt into the scrollback record.
                        r.push_line(format!("{PROMPT}{input}"), false);
                        r.refresh()?;

                        let trimmed = input.trim().to_string();
                        if trimmed.is_empty() {
                            continue;
                        }

                        // Commands and prompts share one dispatcher. Scope the
                        // TuiUi borrow so `r` is free again in the match arms.
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
                                let mut ui = TuiUi { r: &mut r };
                                if let Err(e) = session.send(&trimmed, &mut ui) {
                                    r.push_line(format!("error: {e:#}"), false);
                                }
                                r.commit_pending_if_any();
                                r.refresh()?;
                            }
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
    Ok(())
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
        let bar = pad_to(&self.status, width as usize);
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

/// A [`Ui`] that streams into the renderer's scrollback and keeps the input
/// line pinned below. Reasoning is dimmed, the answer is normal, tool activity
/// is annotated — mirroring [`crate::agent::StdoutUi`].
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
    fn emit_block_commits_fully() {
        let mut r = Renderer::new();
        r.emit_block("⚙ read {}", true);
        assert_eq!(r.scroll.len(), 1);
        assert!(r.pending.is_empty());
    }
}
