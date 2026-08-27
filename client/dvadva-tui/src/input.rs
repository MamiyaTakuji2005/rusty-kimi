//! Single-line input editor with a byte-offset cursor.
//!
//! The TUI's message box: characters insert at the cursor, arrows and
//! Home/End move it, Backspace/Delete remove around it. Multi-line input is
//! out of scope for v1 — pasted newlines become spaces, and `Enter` always
//! submits.

/// A one-line text editor.
#[derive(Default)]
pub struct Editor {
    buf: String,
    /// Byte offset into `buf`, always on a char boundary.
    cursor: usize,
}

impl Editor {
    pub fn clear(&mut self) {
        self.buf.clear();
        self.cursor = 0;
    }

    pub fn text(&self) -> &str {
        &self.buf
    }

    /// Display width of the text before the cursor — the column to draw it
    /// at. CJK and other wide characters occupy two cells, so this is a cell
    /// count, not a char count.
    pub fn cursor_col(&self) -> usize {
        unicode_width::UnicodeWidthStr::width(&self.buf[..self.cursor])
    }

    pub fn insert(&mut self, text: &str) {
        // Pasted newlines would silently become a submit-on-enter mess.
        let flat: String = text
            .chars()
            .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
            .collect();
        self.buf.insert_str(self.cursor, &flat);
        self.cursor += flat.len();
    }

    pub fn backspace(&mut self) {
        if let Some(pos) = self.prev_boundary(self.cursor) {
            self.buf.replace_range(pos..self.cursor, "");
            self.cursor = pos;
        }
    }

    pub fn delete(&mut self) {
        if let Some(end) = self.next_boundary(self.cursor) {
            self.buf.replace_range(self.cursor..end, "");
        }
    }

    pub fn left(&mut self) {
        if let Some(pos) = self.prev_boundary(self.cursor) {
            self.cursor = pos;
        }
    }

    pub fn right(&mut self) {
        if let Some(pos) = self.next_boundary(self.cursor) {
            self.cursor = pos;
        }
    }

    pub fn home(&mut self) {
        self.cursor = 0;
    }

    pub fn end(&mut self) {
        self.cursor = self.buf.len();
    }

    /// Start of the previous char before `pos`, if any.
    fn prev_boundary(&self, pos: usize) -> Option<usize> {
        self.buf[..pos].char_indices().next_back().map(|(i, _)| i)
    }

    /// End of the char starting at `pos`, if any.
    fn next_boundary(&self, pos: usize) -> Option<usize> {
        self.buf[pos..].chars().next().map(|c| pos + c.len_utf8())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_moves_cursor_and_flattens_newlines() {
        let mut ed = Editor::default();
        ed.insert("ab");
        ed.insert("c\nd\r");
        assert_eq!(ed.text(), "abc d ");
        assert_eq!(ed.cursor_col(), 6);
    }

    #[test]
    fn backspace_and_delete_are_char_aware() {
        let mut ed = Editor::default();
        ed.insert("a日b");
        ed.left(); // before b
        ed.backspace(); // removes 日
        assert_eq!(ed.text(), "ab");
        ed.left();
        ed.left();
        ed.delete(); // removes a
        assert_eq!(ed.text(), "b");
        assert_eq!(ed.cursor_col(), 0);
    }

    #[test]
    fn boundaries_clamp_at_ends() {
        let mut ed = Editor::default();
        ed.insert("x");
        ed.right(); // no-op at end
        ed.delete(); // nothing to delete
        ed.backspace(); // removes x
        ed.backspace(); // no-op at start
        assert_eq!(ed.text(), "");
    }

    #[test]
    fn cursor_col_counts_display_width() {
        let mut ed = Editor::default();
        ed.insert("a日本b"); // CJK chars are two cells wide
        assert_eq!(ed.cursor_col(), 6);
        ed.left(); // before b
        ed.left(); // before 本
        assert_eq!(ed.cursor_col(), 3);
    }

    #[test]
    fn home_end_jump() {
        let mut ed = Editor::default();
        ed.insert("hello");
        ed.home();
        ed.insert(">");
        ed.end();
        ed.insert("<");
        assert_eq!(ed.text(), ">hello<");
        assert_eq!(ed.cursor_col(), 7);
    }
}
