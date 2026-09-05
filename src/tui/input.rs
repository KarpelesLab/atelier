//! The single-line input editor for the inline TUI.
//!
//! Factored out from the renderer so the pure editing/windowing logic can be
//! unit-tested without a terminal. Columns are counted as **one per `char`**:
//! there is no Unicode width handling, so wide (CJK / emoji) or zero-width
//! combining characters will mis-place the on-screen cursor. This is a
//! deliberate simplification for a minimal interface — see the module docs.

/// A minimal editable line buffer with a cursor.
///
/// The buffer is stored as `Vec<char>` so the cursor is a plain character
/// index (no byte-boundary juggling). Not a full readline: no kill-ring, word
/// motion, or history — just the editing the roadmap asks for.
#[derive(Default)]
pub(crate) struct LineEditor {
    buf: Vec<char>,
    /// Cursor position as a character index in `0..=buf.len()`.
    cursor: usize,
}

impl LineEditor {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Insert a character at the cursor and advance past it.
    pub(crate) fn insert(&mut self, c: char) {
        self.buf.insert(self.cursor, c);
        self.cursor += 1;
    }

    /// Delete the character before the cursor (Backspace).
    pub(crate) fn backspace(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
            self.buf.remove(self.cursor);
        }
    }

    /// Delete the character under the cursor (Delete).
    pub(crate) fn delete(&mut self) {
        if self.cursor < self.buf.len() {
            self.buf.remove(self.cursor);
        }
    }

    pub(crate) fn left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    pub(crate) fn right(&mut self) {
        if self.cursor < self.buf.len() {
            self.cursor += 1;
        }
    }

    pub(crate) fn home(&mut self) {
        self.cursor = 0;
    }

    pub(crate) fn end(&mut self) {
        self.cursor = self.buf.len();
    }

    pub(crate) fn clear(&mut self) {
        self.buf.clear();
        self.cursor = 0;
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// The current contents as a `String`.
    pub(crate) fn text(&self) -> String {
        self.buf.iter().collect()
    }

    /// Take the current contents, leaving the editor empty.
    pub(crate) fn take(&mut self) -> String {
        let s = self.text();
        self.clear();
        s
    }

    /// Compute the visible slice of the line and the absolute cursor column,
    /// given the total terminal `width` and a leading prompt of `prompt_cols`
    /// columns. The returned text always fits on a single row (it never wraps):
    /// when the line is longer than the available space it is scrolled
    /// horizontally so the cursor stays visible. One trailing column is
    /// reserved so the cursor can sit just past the last character without
    /// triggering the terminal's auto-wrap.
    pub(crate) fn view(&self, width: usize, prompt_cols: usize) -> (String, usize) {
        let avail = width.saturating_sub(prompt_cols).saturating_sub(1).max(1);
        let len = self.buf.len();
        // Anchor the window so the cursor is always within `avail` columns.
        let start = self.cursor.saturating_sub(avail);
        let end = (start + avail).min(len);
        let text: String = self.buf[start..end].iter().collect();
        let col = prompt_cols + (self.cursor - start);
        (text, col)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(ed: &mut LineEditor, s: &str) {
        for c in s.chars() {
            ed.insert(c);
        }
    }

    #[test]
    fn insert_and_text() {
        let mut ed = LineEditor::new();
        feed(&mut ed, "hello");
        assert_eq!(ed.text(), "hello");
        assert!(!ed.is_empty());
    }

    #[test]
    fn insert_in_middle() {
        let mut ed = LineEditor::new();
        feed(&mut ed, "helo");
        ed.home();
        ed.right();
        ed.right();
        ed.insert('l'); // "he" | "lo" -> insert 'l' between -> "hello"
        assert_eq!(ed.text(), "hello");
    }

    #[test]
    fn backspace_and_delete() {
        let mut ed = LineEditor::new();
        feed(&mut ed, "abc");
        ed.backspace(); // "ab"
        assert_eq!(ed.text(), "ab");
        ed.home();
        ed.delete(); // remove 'a' -> "b"
        assert_eq!(ed.text(), "b");
        // backspace at start is a no-op
        ed.home();
        ed.backspace();
        assert_eq!(ed.text(), "b");
    }

    #[test]
    fn cursor_bounds() {
        let mut ed = LineEditor::new();
        feed(&mut ed, "ab");
        // right past the end clamps
        ed.right();
        ed.right();
        ed.right();
        ed.insert('c');
        assert_eq!(ed.text(), "abc");
        // left past the start clamps
        ed.home();
        ed.left();
        ed.left();
        ed.insert('X');
        assert_eq!(ed.text(), "Xabc");
    }

    #[test]
    fn take_clears() {
        let mut ed = LineEditor::new();
        feed(&mut ed, "cmd");
        assert_eq!(ed.take(), "cmd");
        assert!(ed.is_empty());
        assert_eq!(ed.text(), "");
    }

    #[test]
    fn view_fits_within_width() {
        let mut ed = LineEditor::new();
        feed(&mut ed, "hi");
        // prompt of 2 cols, plenty of width: whole line shown, cursor after it.
        let (disp, col) = ed.view(80, 2);
        assert_eq!(disp, "hi");
        assert_eq!(col, 4); // 2 (prompt) + 2 (chars)
    }

    #[test]
    fn view_scrolls_horizontally_keeping_cursor_visible() {
        let mut ed = LineEditor::new();
        feed(&mut ed, "0123456789"); // 10 chars, cursor at end (10)
        // width 6, prompt 2 => avail = 6 - 2 - 1 = 3 columns of text.
        let (disp, col) = ed.view(6, 2);
        assert_eq!(disp.chars().count(), 3);
        // Cursor kept just inside the reserved last column.
        assert_eq!(col, 5); // width - 1
        // The visible window ends at the cursor (tail of the buffer).
        assert_eq!(disp, "789");
    }

    #[test]
    fn view_survives_tiny_width() {
        let mut ed = LineEditor::new();
        feed(&mut ed, "abc");
        // Degenerate widths must not panic.
        let _ = ed.view(0, 2);
        let _ = ed.view(1, 2);
        let _ = ed.view(2, 5);
    }
}
