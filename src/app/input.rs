//! Text-input editing state and operations.
//!
//! The prompt line's editable buffer, cursor, selection, and recall history
//! were previously six loose fields on the `AppState` god-object, with the
//! editing operations inlined into the 630-line key handler. Both now live
//! here: [`InputState`] owns the data, and its methods own the editing logic —
//! insert, delete, cursor movement, selection, and history recall — so the
//! bug-prone parts (byte/char offset juggling, word boundaries, selection
//! deletion) are testable in isolation.

/// A multi-line paste collapsed to a compact placeholder chip in the buffer.
/// The chip keeps the prompt readable; the real text is restored by
/// [`InputState::expanded`] at submit time.
#[derive(Debug, Clone)]
pub struct PasteChunk {
    /// The exact token shown in the buffer, e.g. `[Pasted #1, 42 lines]`.
    pub placeholder: String,
    /// The real pasted text this chip stands in for.
    pub content: String,
}

/// Editable state of the prompt input line.
#[derive(Debug, Default, Clone)]
pub struct InputState {
    /// Current text in the prompt.
    pub buffer: String,
    /// Cursor position as a character index into `buffer`.
    pub cursor: usize,
    /// Anchor of an active selection, if any (character index into `buffer`).
    pub selection_anchor: Option<usize>,
    /// Previously submitted inputs, for up/down recall.
    pub history: Vec<String>,
    /// Position within `history` while recalling; `None` when editing fresh.
    pub history_index: Option<usize>,
    /// Buffer stashed when the user starts browsing history, restored on exit.
    pub unsent: String,
    /// Multi-line pastes collapsed to chips in `buffer`, restored on submit.
    pub pastes: Vec<PasteChunk>,
    /// Monotonic id for the next paste chip (never reused within a draft).
    pub next_paste_id: usize,
}

impl InputState {
    fn char_count(&self) -> usize {
        self.buffer.chars().count()
    }

    /// On a cursor move: extend the selection (shift held) or collapse it.
    fn anchor_for_move(&mut self, extend: bool) {
        if !extend {
            self.selection_anchor = None;
        } else if self.selection_anchor.is_none() {
            self.selection_anchor = Some(self.cursor);
        }
    }

    /// Delete the active selection, if any. Returns whether anything was removed.
    pub fn delete_selection(&mut self) -> bool {
        let Some(anchor) = self.selection_anchor.take() else {
            return false;
        };
        let (start, end) = if anchor <= self.cursor {
            (anchor, self.cursor)
        } else {
            (self.cursor, anchor)
        };
        let bs = char_to_byte_pos(&self.buffer, start);
        let be = char_to_byte_pos(&self.buffer, end);
        self.buffer.drain(bs..be);
        self.cursor = start;
        true
    }

    /// Insert a character at the cursor, replacing any selection first.
    pub fn insert_char(&mut self, c: char) {
        self.delete_selection();
        let byte_pos = char_to_byte_pos(&self.buffer, self.cursor);
        self.buffer.insert(byte_pos, c);
        self.cursor += 1;
        self.selection_anchor = None;
    }

    /// Insert a whole string at the cursor, replacing any selection first.
    /// Used for bracketed paste: the entire clipboard lands as one edit
    /// instead of a stream of key events, so embedded newlines never fire an
    /// early submit. Callers sanitize (strip `\r`, drop `\n` for single-line
    /// fields) before calling.
    pub fn insert_str(&mut self, s: &str) {
        if s.is_empty() {
            return;
        }
        self.delete_selection();
        let byte_pos = char_to_byte_pos(&self.buffer, self.cursor);
        self.buffer.insert_str(byte_pos, s);
        self.cursor += s.chars().count();
        self.selection_anchor = None;
    }

    /// Collapse a multi-line paste to a compact placeholder chip at the cursor
    /// and remember the real content for expansion at submit time. Keeps the
    /// prompt readable and — since the newlines live in the stored content, not
    /// the buffer — stops a pasted block from firing an early submit.
    pub fn insert_paste(&mut self, content: String) {
        self.next_paste_id += 1;
        let id = self.next_paste_id;
        let lines = content.lines().count().max(1);
        let placeholder = format!("[Pasted #{id}, {lines} lines]");
        self.insert_str(&placeholder);
        self.pastes.push(PasteChunk {
            placeholder,
            content,
        });
    }

    /// The buffer with every still-present paste chip expanded back to its real
    /// content. Used at submit time. A chip the user edited away no longer
    /// matches and is left as-is; unreferenced chunks are simply ignored.
    pub fn expanded(&self) -> String {
        let mut out = self.buffer.clone();
        for chunk in &self.pastes {
            if out.contains(&chunk.placeholder) {
                out = out.replace(&chunk.placeholder, &chunk.content);
            }
        }
        out
    }

    /// Reset the editable buffer (text, cursor, selection, paste chips) without
    /// touching recall history. Called after a submit and on history recall,
    /// where a fresh buffer replaces the draft.
    pub fn clear_buffer(&mut self) {
        self.buffer.clear();
        self.cursor = 0;
        self.selection_anchor = None;
        self.pastes.clear();
    }

    /// Insert a newline at the cursor, replacing any selection first.
    pub fn insert_newline(&mut self) {
        self.delete_selection();
        let byte_pos = char_to_byte_pos(&self.buffer, self.cursor);
        self.buffer.insert(byte_pos, '\n');
        self.cursor += 1;
        self.selection_anchor = None;
    }

    /// Delete the character before the cursor, or the selection if there is one.
    pub fn backspace(&mut self) {
        if self.selection_anchor.is_some() {
            self.delete_selection();
            return;
        }
        let char_count = self.char_count();
        if self.cursor > 0 && self.cursor <= char_count {
            self.cursor -= 1;
            let byte_pos = char_to_byte_pos(&self.buffer, self.cursor);
            if byte_pos < self.buffer.len() {
                self.buffer.remove(byte_pos);
            }
        }
    }

    /// Delete the character at the cursor, or the selection if there is one.
    pub fn delete_forward(&mut self) {
        if self.selection_anchor.is_some() {
            self.delete_selection();
            return;
        }
        if self.cursor < self.char_count() {
            let byte_pos = char_to_byte_pos(&self.buffer, self.cursor);
            if byte_pos < self.buffer.len() {
                self.buffer.remove(byte_pos);
            }
        }
    }

    /// Move the cursor left by one character, or one word when `word` is set.
    pub fn move_left(&mut self, extend: bool, word: bool) {
        self.anchor_for_move(extend);
        let clamped = self.cursor.min(self.char_count());
        self.cursor = if word {
            prev_word_start(&self.buffer, clamped)
        } else {
            clamped.saturating_sub(1)
        };
    }

    /// Move the cursor right by one character, or one word when `word` is set.
    pub fn move_right(&mut self, extend: bool, word: bool) {
        self.anchor_for_move(extend);
        let char_count = self.char_count();
        self.cursor = if word {
            next_word_end(&self.buffer, self.cursor)
        } else if self.cursor < char_count {
            self.cursor + 1
        } else {
            self.cursor
        };
    }

    /// Move the cursor to the start of the buffer.
    pub fn move_home(&mut self, extend: bool) {
        self.anchor_for_move(extend);
        self.cursor = 0;
    }

    /// Move the cursor to the end of the buffer.
    pub fn move_end(&mut self, extend: bool) {
        self.anchor_for_move(extend);
        self.cursor = self.char_count();
    }

    /// Select the entire buffer.
    pub fn select_all(&mut self) {
        self.selection_anchor = Some(0);
        self.cursor = self.char_count();
    }

    /// Recall the previous (older) history entry. On first recall the current
    /// buffer is stashed in `unsent`.
    pub fn history_prev(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let idx = match self.history_index {
            None => {
                self.unsent = self.buffer.clone();
                Some(self.history.len() - 1)
            }
            Some(i) if i > 0 => Some(i - 1),
            _ => None,
        };
        if let Some(i) = idx {
            self.buffer = self.history[i].clone();
            self.cursor = self.char_count();
            self.selection_anchor = None;
            self.pastes.clear();
            self.history_index = Some(i);
        }
    }

    /// Recall the next (newer) history entry, restoring the stashed `unsent`
    /// buffer once past the most recent entry.
    pub fn history_next(&mut self) {
        let next = self.history_index.map(|i| i + 1);
        match next {
            Some(i) if i < self.history.len() => {
                self.buffer = self.history[i].clone();
                self.cursor = self.char_count();
                self.history_index = Some(i);
            }
            _ => {
                self.buffer = std::mem::take(&mut self.unsent);
                self.cursor = self.char_count();
                self.history_index = None;
            }
        }
        self.selection_anchor = None;
        self.pastes.clear();
    }
}

/// Byte offset of the `char_pos`-th character, or the string's length if
/// `char_pos` is past the end.
pub fn char_to_byte_pos(s: &str, char_pos: usize) -> usize {
    s.char_indices()
        .nth(char_pos)
        .map(|(i, _)| i)
        .unwrap_or(s.len())
}

fn char_is_word(ch: char) -> bool {
    ch.is_alphanumeric() || ch == '_'
}

fn prev_word_start(s: &str, cursor: usize) -> usize {
    if cursor == 0 {
        return 0;
    }
    let chars: Vec<char> = s.chars().collect();
    let mut i = cursor;
    while i > 0 && chars[i - 1].is_whitespace() {
        i -= 1;
    }
    if i == 0 {
        return 0;
    }
    let word = char_is_word(chars[i - 1]);
    while i > 0 && char_is_word(chars[i - 1]) == word && !chars[i - 1].is_whitespace() {
        i -= 1;
    }
    i
}

fn next_word_end(s: &str, cursor: usize) -> usize {
    let total = s.chars().count();
    if cursor >= total {
        return total;
    }
    let chars: Vec<char> = s.chars().collect();
    let mut i = cursor;
    while i < total && chars[i].is_whitespace() {
        i += 1;
    }
    if i >= total {
        return total;
    }
    let word = char_is_word(chars[i]);
    while i < total && !chars[i].is_whitespace() && char_is_word(chars[i]) == word {
        i += 1;
    }
    i
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(buffer: &str, cursor: usize) -> InputState {
        InputState {
            buffer: buffer.to_string(),
            cursor,
            ..Default::default()
        }
    }

    #[test]
    fn insert_char_mid_buffer() {
        let mut s = at("ac", 1);
        s.insert_char('b');
        assert_eq!(s.buffer, "abc");
        assert_eq!(s.cursor, 2);
    }

    #[test]
    fn insert_char_replaces_selection() {
        let mut s = at("hello", 5);
        s.selection_anchor = Some(0);
        s.insert_char('X');
        assert_eq!(s.buffer, "X");
        assert_eq!(s.cursor, 1);
        assert_eq!(s.selection_anchor, None);
    }

    #[test]
    fn insert_char_multibyte() {
        // Cursor is a char index; inserting after a 4-byte emoji must not panic.
        let mut s = at("😀b", 1);
        s.insert_char('a');
        assert_eq!(s.buffer, "😀ab");
        assert_eq!(s.cursor, 2);
    }

    #[test]
    fn insert_str_mid_buffer_advances_by_char_count() {
        let mut s = at("ac", 1);
        s.insert_str("XYZ");
        assert_eq!(s.buffer, "aXYZc");
        assert_eq!(s.cursor, 4);
    }

    #[test]
    fn insert_str_replaces_selection() {
        let mut s = at("hello", 5);
        s.selection_anchor = Some(0);
        s.insert_str("hi");
        assert_eq!(s.buffer, "hi");
        assert_eq!(s.cursor, 2);
        assert_eq!(s.selection_anchor, None);
    }

    #[test]
    fn insert_str_multibyte_and_newlines() {
        // Cursor is a char index; a paste with emoji + newline must count chars,
        // not bytes, and keep the newline in the buffer.
        let mut s = at("😀b", 1);
        s.insert_str("a\nx");
        assert_eq!(s.buffer, "😀a\nxb");
        assert_eq!(s.cursor, 4); // 'a', '\n', 'x' are 3 chars past the emoji
    }

    #[test]
    fn insert_str_empty_is_noop() {
        let mut s = at("ab", 1);
        s.insert_str("");
        assert_eq!(s.buffer, "ab");
        assert_eq!(s.cursor, 1);
    }

    #[test]
    fn insert_paste_collapses_to_chip_and_expands() {
        let mut s = at("hi ", 3);
        s.insert_paste("line one\nline two\nline three".to_string());
        assert_eq!(s.buffer, "hi [Pasted #1, 3 lines]");
        // Cursor sits after the chip; the newlines never entered the buffer.
        assert!(!s.buffer.contains('\n'));
        assert_eq!(s.expanded(), "hi line one\nline two\nline three");
    }

    #[test]
    fn insert_paste_ids_increment_and_both_expand() {
        let mut s = at("", 0);
        s.insert_paste("a\nb".to_string());
        s.insert_str(" and ");
        s.insert_paste("c\nd".to_string());
        assert_eq!(s.buffer, "[Pasted #1, 2 lines] and [Pasted #2, 2 lines]");
        assert_eq!(s.expanded(), "a\nb and c\nd");
    }

    #[test]
    fn expanded_leaves_edited_away_chip_literal() {
        let mut s = at("", 0);
        s.insert_paste("x\ny".to_string());
        // User deletes the chip entirely; expansion has nothing to substitute.
        s.clear_buffer();
        s.insert_str("plain text");
        assert_eq!(s.expanded(), "plain text");
        assert!(s.pastes.is_empty());
    }

    #[test]
    fn history_recall_drops_paste_chips() {
        let mut s = at("draft ", 6);
        s.history = vec!["old".to_string()];
        s.insert_paste("a\nb".to_string());
        assert!(!s.pastes.is_empty());
        s.history_prev();
        assert_eq!(s.buffer, "old");
        assert!(s.pastes.is_empty());
    }

    #[test]
    fn backspace_at_start_is_noop() {
        let mut s = at("hi", 0);
        s.backspace();
        assert_eq!(s.buffer, "hi");
        assert_eq!(s.cursor, 0);
    }

    #[test]
    fn backspace_removes_previous_char() {
        let mut s = at("abc", 2);
        s.backspace();
        assert_eq!(s.buffer, "ac");
        assert_eq!(s.cursor, 1);
    }

    #[test]
    fn backspace_deletes_selection() {
        let mut s = at("abcd", 3);
        s.selection_anchor = Some(1);
        s.backspace();
        assert_eq!(s.buffer, "ad");
        assert_eq!(s.cursor, 1);
        assert_eq!(s.selection_anchor, None);
    }

    #[test]
    fn delete_forward_at_end_is_noop() {
        let mut s = at("hi", 2);
        s.delete_forward();
        assert_eq!(s.buffer, "hi");
    }

    #[test]
    fn delete_forward_removes_char_at_cursor() {
        let mut s = at("abc", 1);
        s.delete_forward();
        assert_eq!(s.buffer, "ac");
        assert_eq!(s.cursor, 1);
    }

    #[test]
    fn move_left_clamps_and_clears_selection() {
        let mut s = at("ab", 0);
        s.selection_anchor = Some(2);
        s.move_left(false, false);
        assert_eq!(s.cursor, 0); // saturating
        assert_eq!(s.selection_anchor, None);
    }

    #[test]
    fn move_right_word_jumps_to_word_end() {
        let mut s = at("foo bar", 0);
        s.move_right(false, true);
        assert_eq!(s.cursor, 3); // end of "foo"
    }

    #[test]
    fn move_left_word_jumps_to_word_start() {
        let mut s = at("foo bar", 7);
        s.move_left(false, true);
        assert_eq!(s.cursor, 4); // start of "bar"
    }

    #[test]
    fn shift_move_sets_then_keeps_anchor() {
        let mut s = at("hello", 5);
        s.move_left(true, false);
        assert_eq!(s.selection_anchor, Some(5)); // anchored at original cursor
        assert_eq!(s.cursor, 4);
        s.move_left(true, false);
        assert_eq!(s.selection_anchor, Some(5)); // anchor preserved
        assert_eq!(s.cursor, 3);
    }

    #[test]
    fn select_all_and_home_end() {
        let mut s = at("hello", 2);
        s.select_all();
        assert_eq!(s.selection_anchor, Some(0));
        assert_eq!(s.cursor, 5);
        s.move_home(false);
        assert_eq!(s.cursor, 0);
        assert_eq!(s.selection_anchor, None);
        s.move_end(false);
        assert_eq!(s.cursor, 5);
    }

    #[test]
    fn history_recall_cycles_and_restores_unsent() {
        let mut s = at("draft", 5);
        s.history = vec!["first".to_string(), "second".to_string()];

        s.history_prev(); // stash "draft", recall newest "second"
        assert_eq!(s.buffer, "second");
        assert_eq!(s.history_index, Some(1));
        assert_eq!(s.unsent, "draft");

        s.history_prev(); // older
        assert_eq!(s.buffer, "first");
        assert_eq!(s.history_index, Some(0));

        s.history_next(); // back to "second"
        assert_eq!(s.buffer, "second");
        assert_eq!(s.history_index, Some(1));

        s.history_next(); // past newest → restore the stashed draft
        assert_eq!(s.buffer, "draft");
        assert_eq!(s.history_index, None);
    }

    #[test]
    fn history_prev_on_empty_is_noop() {
        let mut s = at("x", 1);
        s.history_prev();
        assert_eq!(s.buffer, "x");
        assert_eq!(s.history_index, None);
    }
}
