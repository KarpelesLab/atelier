//! Full-screen configuration editor, shown on `/config` (alternate screen).
//!
//! # Contract (stable — the implementer owns `src/tui/config.rs`)
//!
//! `run(session) -> Result<()>` takes over the terminal (enter the alternate
//! screen; raw mode is already on via the caller's `RawGuard`), renders an
//! editable list of settings, and returns when the user saves & exits (Esc /
//! `q`) or cancels. Read live values from the `Session` accessors
//! (`config().model`, `auto_approve()`, `context_limit()`, `review_enabled()`,
//! `settings()`), and apply edits with the setters
//! (`set_default_model`/`set_auto_approve`/`set_context_limit`/`set_review_enabled`),
//! then `session.save_settings()`. Restore the main screen on every exit path.
//! Do not edit files outside `src/tui/`.

use std::io::{self, Write};

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind};
use crossterm::style::{Attribute, Print, SetAttribute};
use crossterm::terminal::{Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::{cursor, execute, queue, terminal};

use crate::agent::Session;

/// Which setting a row edits. The order here is the on-screen order.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Row {
    Model,
    AutoApprove,
    ContextLimit,
    ReviewMode,
}

impl Row {
    /// Rows in display order.
    const ALL: [Row; 4] = [
        Row::Model,
        Row::AutoApprove,
        Row::ContextLimit,
        Row::ReviewMode,
    ];

    fn label(self) -> &'static str {
        match self {
            Row::Model => "model",
            Row::AutoApprove => "auto-approve",
            Row::ContextLimit => "context limit",
            Row::ReviewMode => "review mode",
        }
    }

    /// A bool row toggles in place; a value row opens an inline text editor.
    fn is_bool(self) -> bool {
        matches!(self, Row::AutoApprove | Row::ReviewMode)
    }
}

/// The editable settings, held locally while the screen is open and applied to
/// the `Session` on exit.
struct State {
    model: String,
    auto_approve: bool,
    context_limit: u32,
    review: bool,
    /// Currently highlighted row.
    selected: usize,
    /// When editing a value row, the in-progress text buffer.
    editing: Option<String>,
}

impl State {
    fn from_session(session: &Session) -> Self {
        Self {
            model: session.config().model.clone(),
            auto_approve: session.auto_approve(),
            context_limit: session.context_limit(),
            review: session.review_enabled(),
            selected: 0,
            editing: None,
        }
    }

    /// The value column for a row, rendered as text.
    fn value_text(&self, row: Row) -> String {
        match row {
            Row::Model => self.model.clone(),
            Row::AutoApprove => bool_text(self.auto_approve).to_string(),
            Row::ContextLimit => self.context_limit.to_string(),
            Row::ReviewMode => bool_text(self.review).to_string(),
        }
    }

    /// Toggle the bool at the current row (no-op on a value row).
    fn toggle(&mut self, row: Row) {
        match row {
            Row::AutoApprove => self.auto_approve = !self.auto_approve,
            Row::ReviewMode => self.review = !self.review,
            _ => {}
        }
    }

    /// Begin editing the current value row, seeding the buffer with its text.
    fn begin_edit(&mut self, row: Row) {
        self.editing = Some(self.value_text(row));
    }

    /// Confirm the in-progress edit, writing the buffer back to the row.
    fn commit_edit(&mut self, row: Row) {
        let Some(buf) = self.editing.take() else {
            return;
        };
        match row {
            Row::Model => {
                let trimmed = buf.trim();
                if !trimmed.is_empty() {
                    self.model = trimmed.to_string();
                }
            }
            Row::ContextLimit => self.context_limit = parse_context_limit(&buf),
            _ => {}
        }
    }
}

fn bool_text(b: bool) -> &'static str {
    if b { "on" } else { "off" }
}

/// Parse a context-limit edit buffer into a value clamped to `>= 1`. Non-digit
/// characters are ignored (the editor already rejects them, but this stays
/// robust); an empty/zero result clamps up to `1`.
fn parse_context_limit(s: &str) -> u32 {
    let digits: String = s.chars().filter(|c| c.is_ascii_digit()).collect();
    digits.parse::<u32>().unwrap_or(0).max(1)
}

/// Enters the alternate screen and hides the cursor on construction; its `Drop`
/// leaves the alternate screen and shows the cursor again — so every exit path,
/// including a mid-function `?` early return or a panic, restores the terminal.
struct ScreenGuard;

impl ScreenGuard {
    fn enter() -> io::Result<Self> {
        execute!(io::stdout(), EnterAlternateScreen, cursor::Hide)?;
        Ok(ScreenGuard)
    }
}

impl Drop for ScreenGuard {
    fn drop(&mut self) {
        let mut out = io::stdout();
        let _ = execute!(
            out,
            SetAttribute(Attribute::Reset),
            cursor::Show,
            LeaveAlternateScreen
        );
        let _ = out.flush();
    }
}

/// Run the full-screen settings editor. Reads live values from the `Session`,
/// lets the user edit them, applies the changes via the setters and persists
/// them on exit. The alternate screen is always restored (see [`ScreenGuard`]).
pub fn run(session: &mut Session) -> Result<()> {
    // The guard owns the alternate screen for the whole function: any `?` below
    // still runs its `Drop`, so the user never gets stuck in the alt screen.
    let _guard = ScreenGuard::enter()?;

    let mut state = State::from_session(session);
    draw(&state)?;

    loop {
        match event::read()? {
            Event::Resize(_, _) => draw(&state)?,
            Event::Key(k) if k.kind != KeyEventKind::Release => {
                if handle_key(&mut state, k) {
                    break;
                }
                draw(&state)?;
            }
            _ => {}
        }
    }

    apply(session, &state);
    // A save failure is surfaced on the (still-alternate) screen rather than
    // panicking or bubbling up; the guard restores the main screen on return.
    if let Err(e) = session.save_settings() {
        report_save_error(&e);
    }

    Ok(())
}

/// Handle one key. Returns `true` when the editor should save & exit.
fn handle_key(state: &mut State, k: KeyEvent) -> bool {
    let row = Row::ALL[state.selected];

    // --- Inline edit mode: keys edit the buffer, never the top-level nav. ---
    if let Some(buf) = state.editing.as_mut() {
        match k.code {
            KeyCode::Esc => {
                // Cancel the edit; the value is left unchanged.
                state.editing = None;
            }
            KeyCode::Enter => state.commit_edit(row),
            KeyCode::Backspace => {
                buf.pop();
            }
            KeyCode::Char(c) => {
                // The number row only accepts digits.
                if row == Row::ContextLimit {
                    if c.is_ascii_digit() {
                        buf.push(c);
                    }
                } else {
                    buf.push(c);
                }
            }
            _ => {}
        }
        return false;
    }

    // --- Top-level navigation. ---
    match k.code {
        KeyCode::Up | KeyCode::Char('k') => {
            if state.selected == 0 {
                state.selected = Row::ALL.len() - 1;
            } else {
                state.selected -= 1;
            }
        }
        KeyCode::Down | KeyCode::Char('j') => {
            state.selected = (state.selected + 1) % Row::ALL.len();
        }
        KeyCode::Char(' ') => {
            if row.is_bool() {
                state.toggle(row);
            }
        }
        KeyCode::Enter => {
            if row.is_bool() {
                state.toggle(row);
            } else {
                state.begin_edit(row);
            }
        }
        // Esc / q / s at the top level all save & exit.
        KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('s') => return true,
        _ => {}
    }
    false
}

/// Apply the edited values back onto the session (unconditionally — the setters
/// are cheap and idempotent).
fn apply(session: &mut Session, state: &State) {
    session.set_default_model(&state.model);
    session.set_auto_approve(state.auto_approve);
    session.set_context_limit(state.context_limit);
    session.set_review_enabled(state.review);
}

/// The screen title.
const TITLE: &str = "atelier · settings";
/// Column where the value text starts (labels are left-padded into this gutter).
const VALUE_COL: usize = 18;

/// Repaint the whole screen. Robust to tiny terminals (rows that don't fit are
/// simply not drawn; nothing panics).
fn draw(state: &State) -> io::Result<()> {
    let (cols, rows) = terminal::size().unwrap_or((80, 24));
    let width = cols.max(1) as usize;
    let mut out = io::stdout();

    queue!(
        out,
        cursor::Hide,
        Clear(ClearType::All),
        cursor::MoveTo(0, 0)
    )?;

    // Title bar (reverse video, full width).
    let mut line: u16 = 0;
    put_line(
        &mut out,
        &mut line,
        rows,
        &pad_to(&format!(" {TITLE}"), width),
        true,
    )?;
    put_blank(&mut out, &mut line, rows)?;

    for (i, &row) in Row::ALL.iter().enumerate() {
        let selected = i == state.selected;
        let editing = selected && state.editing.is_some();
        let value = match &state.editing {
            Some(buf) if selected => format!("{buf}▌"),
            _ => state.value_text(row),
        };
        let label = format!(
            "{:>width$}",
            row.label(),
            width = VALUE_COL.saturating_sub(2)
        );
        let text = format!("  {label}  {value}");
        // Highlight the selected row (reverse video) unless we're actively
        // editing it, where a caret already marks the focus.
        put_line(
            &mut out,
            &mut line,
            rows,
            &pad_to(&text, width),
            selected && !editing,
        )?;
    }

    put_blank(&mut out, &mut line, rows)?;

    let hint = if state.editing.is_some() {
        "type to edit · Enter confirm · Esc cancel"
    } else {
        "↑/↓ move · Enter edit/toggle · Space toggle · s/Esc/q save & exit"
    };
    put_line(
        &mut out,
        &mut line,
        rows,
        &pad_to(&format!(" {hint}"), width),
        true,
    )?;

    out.flush()
}

/// Print one line at the current row if it fits, then advance the row cursor.
/// `reverse` renders it in reverse video (for the title/footer/selection).
fn put_line(
    out: &mut io::Stdout,
    line: &mut u16,
    rows: u16,
    text: &str,
    reverse: bool,
) -> io::Result<()> {
    if *line >= rows {
        *line += 1;
        return Ok(());
    }
    queue!(out, cursor::MoveTo(0, *line))?;
    if reverse {
        queue!(out, SetAttribute(Attribute::Reverse))?;
    }
    queue!(out, Print(text), SetAttribute(Attribute::Reset))?;
    *line += 1;
    Ok(())
}

fn put_blank(out: &mut io::Stdout, line: &mut u16, rows: u16) -> io::Result<()> {
    put_line(out, line, rows, "", false)
}

/// Clip `s` to `width` columns and right-pad with spaces to exactly fill it, so
/// a reverse-video bar spans the terminal.
fn pad_to(s: &str, width: usize) -> String {
    let mut out: String = s.chars().take(width).collect();
    let len = out.chars().count();
    if len < width {
        out.push_str(&" ".repeat(width - len));
    }
    out
}

/// Surface a save error onto the alternate screen and wait for a keypress, so
/// the user isn't dropped back with a silent failure. Best-effort; ignores I/O
/// errors (the guard still restores the main screen).
fn report_save_error(e: &anyhow::Error) {
    let mut out = io::stdout();
    let msg = format!(" ! failed to save settings: {e}  (press any key)");
    let _ = execute!(
        out,
        cursor::MoveTo(0, 0),
        Clear(ClearType::CurrentLine),
        SetAttribute(Attribute::Reverse),
        Print(msg),
        SetAttribute(Attribute::Reset)
    );
    let _ = out.flush();
    // Consume one key so the message is readable; tolerate read failures.
    loop {
        match event::read() {
            Ok(Event::Key(k)) if k.kind != KeyEventKind::Release => break,
            Ok(_) => {}
            Err(_) => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_limit_clamps_and_filters() {
        assert_eq!(parse_context_limit("100"), 100);
        // Zero and empty clamp up to 1.
        assert_eq!(parse_context_limit("0"), 1);
        assert_eq!(parse_context_limit(""), 1);
        // Non-digits are ignored; the digits that remain parse.
        assert_eq!(parse_context_limit("1a2b3"), 123);
        assert_eq!(parse_context_limit("abc"), 1);
    }

    #[test]
    fn bool_text_renders() {
        assert_eq!(bool_text(true), "on");
        assert_eq!(bool_text(false), "off");
    }

    #[test]
    fn pad_to_fills_and_clips() {
        assert_eq!(pad_to("ab", 5), "ab   ");
        assert_eq!(pad_to("abcdef", 3), "abc");
        assert_eq!(pad_to("x", 0), "");
    }

    #[test]
    fn row_kinds() {
        assert!(Row::AutoApprove.is_bool());
        assert!(Row::ReviewMode.is_bool());
        assert!(!Row::Model.is_bool());
        assert!(!Row::ContextLimit.is_bool());
    }
}
