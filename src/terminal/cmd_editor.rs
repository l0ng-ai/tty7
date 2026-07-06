//! A small, self-contained command-line editor buffer for the prompt.
//!
//! Why not reuse `gpui_component::InputState`? Because it claims `tab`, `up`,
//! `down`, and other keys in its `"Input"` key context, and gpui dispatches
//! keybinding actions *before* `on_key_down` listeners — so an ancestor can't
//! intercept those keys to drive Tab completion / history recall. To own every
//! key at the prompt (the prerequisite for completion, history, syntax
//! highlighting and ghost suggestions) we keep keyboard focus on the terminal and
//! run our own line editor here.
//!
//! The buffer is a `Vec<char>` with a char-index cursor, so cursor arithmetic and
//! word motion never split a multi-byte UTF-8 scalar. It is deliberately
//! editing-only (no rendering, no key mapping); the view owns those.

/// An editable single line plus a cursor position (a char index in `0..=len`),
/// and an optional selection anchor (the selection spans `anchor..cursor`).
#[derive(Default)]
pub struct CmdEditor {
    chars: Vec<char>,
    cursor: usize,
    anchor: Option<usize>,
    /// Undo / redo stacks of `(chars, cursor)` snapshots. Each mutating edit
    /// records the pre-edit state (deduplicated by content) onto `undo`; undo/redo
    /// shuttle states between the two.
    undo: Vec<(Vec<char>, usize)>,
    redo: Vec<(Vec<char>, usize)>,
}

/// Cap on undo history, so a long editing session can't grow it without bound.
const UNDO_LIMIT: usize = 200;

impl CmdEditor {
    pub fn new() -> Self {
        Self::default()
    }

    /// The current line as a `String`.
    pub fn text(&self) -> String {
        self.chars.iter().collect()
    }

    pub fn is_empty(&self) -> bool {
        self.chars.is_empty()
    }

    /// Number of chars in the line (cursor is in `0..=len`).
    pub fn len(&self) -> usize {
        self.chars.len()
    }

    /// Cursor position as a char index (`0..=len`). Used by tests and the
    /// upcoming completion increment.
    #[allow(dead_code)]
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// Cursor position as a byte offset into `text()`, for callers that need to
    /// slice the rendered string (e.g. to split it at the caret).
    #[allow(dead_code)]
    pub fn cursor_byte(&self) -> usize {
        self.chars[..self.cursor].iter().map(|c| c.len_utf8()).sum()
    }

    // ---- Undo / redo ----

    /// Record the current state onto the undo stack (deduplicated by content) and
    /// clear redo. Called at the start of every mutating edit; nested calls within
    /// one edit collapse to a single entry via the content check.
    fn checkpoint(&mut self) {
        if self.undo.last().map(|(c, _)| c.as_slice()) != Some(self.chars.as_slice()) {
            self.undo.push((self.chars.clone(), self.cursor));
            if self.undo.len() > UNDO_LIMIT {
                self.undo.remove(0);
            }
        }
        self.redo.clear();
    }

    pub fn undo(&mut self) {
        // Skip "phantom" checkpoints whose text already equals the current buffer.
        // A no-op edit (Backspace at column 0, Ctrl-K at line end, Ctrl-W at
        // column 0, …) still calls `checkpoint()`, recording the pre-edit state —
        // which for a no-op is identical to the current one. Undoing that entry
        // would be a dead keypress that "restores" the same text instead of the
        // real edit before it. Drop past any such entries to the first checkpoint
        // that actually changes the text. Comparing text alone is enough: plain
        // caret motion never checkpoints, so a top entry matching the current text
        // can only be a no-op's phantom, never a cursor-only undo target.
        while self
            .undo
            .last()
            .is_some_and(|(chars, _)| chars.as_slice() == self.chars.as_slice())
        {
            self.undo.pop();
        }
        if let Some((chars, cursor)) = self.undo.pop() {
            self.redo.push((self.chars.clone(), self.cursor));
            self.chars = chars;
            self.cursor = cursor.min(self.chars.len());
            self.anchor = None;
        }
    }

    pub fn redo(&mut self) {
        if let Some((chars, cursor)) = self.redo.pop() {
            self.undo.push((self.chars.clone(), self.cursor));
            self.chars = chars;
            self.cursor = cursor.min(self.chars.len());
            self.anchor = None;
        }
    }

    /// Insert a string at the cursor, advancing past it. Replaces the selection
    /// first if there is one. Used for typed text and IME-committed text alike.
    pub fn insert_str(&mut self, s: &str) {
        self.checkpoint();
        self.delete_selection();
        for c in s.chars() {
            self.chars.insert(self.cursor, c);
            self.cursor += 1;
        }
    }

    /// Insert a string at the start of the line, leaving the caret (and any
    /// selection) on the characters they were on — their indices shift by the
    /// inserted length. Used to adopt gap typeahead, which was typed
    /// chronologically before the editor's current content.
    pub fn prepend_str(&mut self, s: &str) {
        if s.is_empty() {
            return;
        }
        self.checkpoint();
        let n = s.chars().count();
        for (i, c) in s.chars().enumerate() {
            self.chars.insert(i, c);
        }
        self.cursor += n;
        self.anchor = self.anchor.map(|a| a + n);
    }

    /// Delete the char before the cursor (Backspace), or the selection if any.
    pub fn backspace(&mut self) {
        self.checkpoint();
        if self.delete_selection() {
            return;
        }
        if self.cursor > 0 {
            self.cursor -= 1;
            self.chars.remove(self.cursor);
        }
    }

    /// Delete the char at the cursor (Delete), or the selection if any.
    pub fn delete(&mut self) {
        self.checkpoint();
        if self.delete_selection() {
            return;
        }
        if self.cursor < self.chars.len() {
            self.chars.remove(self.cursor);
        }
    }

    // ---- Selection ----

    /// The selected range as normalized `(start, end)` char indices, or `None`
    /// when there's no (non-empty) selection.
    pub fn selection(&self) -> Option<(usize, usize)> {
        // Clamp both endpoints to the current length. A delete that shrinks the
        // buffer without touching the anchor (delete_word_left/right,
        // delete_to_start/end never clear it) can leave the anchor past the new
        // end; slicing `chars[a..cursor]` on that stale anchor then panics
        // (reachable from real input: shift-select, Alt+Delete, then Cmd+C / Cmd+X,
        // which read `selected_text()`). Clamping is a no-op for every valid state
        // and collapses a deleted-region selection to `None`.
        let n = self.chars.len();
        let a = self.anchor?.min(n);
        let c = self.cursor.min(n);
        if a == c {
            None
        } else {
            Some((a.min(c), a.max(c)))
        }
    }

    /// The selected text, if any.
    pub fn selected_text(&self) -> Option<String> {
        let (s, e) = self.selection()?;
        Some(self.chars[s..e].iter().collect())
    }

    pub fn clear_selection(&mut self) {
        self.anchor = None;
    }

    /// Start a selection at the current cursor if none is active (used before an
    /// extending, shift-modified motion).
    pub fn begin_selection(&mut self) {
        if self.anchor.is_none() {
            self.anchor = Some(self.cursor);
        }
    }

    /// Delete the selection if there is one; returns whether anything was deleted.
    pub fn delete_selection(&mut self) -> bool {
        if let Some((s, e)) = self.selection() {
            self.checkpoint();
            self.chars.drain(s..e);
            self.cursor = s;
            self.anchor = None;
            true
        } else {
            self.anchor = None;
            false
        }
    }

    /// Select the whole line.
    pub fn select_all(&mut self) {
        self.anchor = Some(0);
        self.cursor = self.chars.len();
    }

    /// Select the word (run of non-whitespace) containing char index `idx`.
    pub fn select_word_at(&mut self, idx: usize) {
        let idx = idx.min(self.chars.len());
        let mut s = idx;
        while s > 0 && !self.chars[s - 1].is_whitespace() {
            s -= 1;
        }
        let mut e = idx;
        while e < self.chars.len() && !self.chars[e].is_whitespace() {
            e += 1;
        }
        self.anchor = Some(s);
        self.cursor = e;
    }

    /// Move the cursor to char index `idx` (clamped), extending the selection from
    /// the existing anchor (starting one at the old cursor if needed). For drags.
    pub fn extend_to(&mut self, idx: usize) {
        self.begin_selection();
        self.cursor = idx.min(self.chars.len());
    }

    pub fn move_left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    pub fn move_right(&mut self) {
        if self.cursor < self.chars.len() {
            self.cursor += 1;
        }
    }

    pub fn move_home(&mut self) {
        self.cursor = 0;
    }

    /// Place the cursor at char index `idx` (clamped to the line length). Used to
    /// reposition the caret from a mouse click.
    pub fn set_cursor(&mut self, idx: usize) {
        self.cursor = idx.min(self.chars.len());
    }

    pub fn move_end(&mut self) {
        self.cursor = self.chars.len();
    }

    /// Move left to the start of the previous word (skip trailing whitespace, then
    /// the word). Word = run of non-whitespace.
    pub fn move_word_left(&mut self) {
        while self.cursor > 0 && self.chars[self.cursor - 1].is_whitespace() {
            self.cursor -= 1;
        }
        while self.cursor > 0 && !self.chars[self.cursor - 1].is_whitespace() {
            self.cursor -= 1;
        }
    }

    /// Move right to the end of the next word.
    pub fn move_word_right(&mut self) {
        let n = self.chars.len();
        while self.cursor < n && self.chars[self.cursor].is_whitespace() {
            self.cursor += 1;
        }
        while self.cursor < n && !self.chars[self.cursor].is_whitespace() {
            self.cursor += 1;
        }
    }

    /// Shift the selection anchor to account for the removal of chars `[s, e)`,
    /// exactly as any editor adjusts marks across an edit: an anchor past the
    /// hole moves left by its width, one inside collapses to its start. Without
    /// this, a range delete that doesn't reach the buffer end leaves the anchor
    /// pointing at *shifted* text — `selection()`'s clamp then reports a
    /// phantom selection over chars the user never selected (and ⌘C copies it).
    fn shift_anchor_for_removal(&mut self, s: usize, e: usize) {
        if let Some(a) = self.anchor {
            self.anchor = Some(if a <= s { a } else { a.max(e) - (e - s) });
        }
    }

    /// Delete the word after the cursor (Alt+Delete): skip following whitespace,
    /// then the word.
    pub fn delete_word_right(&mut self) {
        self.checkpoint();
        let n = self.chars.len();
        let mut e = self.cursor;
        while e < n && self.chars[e].is_whitespace() {
            e += 1;
        }
        while e < n && !self.chars[e].is_whitespace() {
            e += 1;
        }
        self.chars.drain(self.cursor..e);
        self.shift_anchor_for_removal(self.cursor, e);
    }

    /// Delete the word before the cursor (Ctrl+W / Alt+Backspace).
    pub fn delete_word_left(&mut self) {
        self.checkpoint();
        let end = self.cursor;
        self.move_word_left();
        self.chars.drain(self.cursor..end);
        self.shift_anchor_for_removal(self.cursor, end);
    }

    /// Delete from the cursor to the start of the line (Ctrl+U / Cmd+Backspace).
    pub fn delete_to_start(&mut self) {
        self.checkpoint();
        self.chars.drain(0..self.cursor);
        let end = self.cursor;
        self.cursor = 0;
        self.shift_anchor_for_removal(0, end);
    }

    /// Delete from the cursor to the end of the line (Ctrl+K).
    pub fn delete_to_end(&mut self) {
        self.checkpoint();
        let end = self.chars.len();
        self.chars.drain(self.cursor..);
        self.shift_anchor_for_removal(self.cursor, end);
    }

    /// Clear the line and reset the cursor and undo history (after submit).
    pub fn clear(&mut self) {
        self.chars.clear();
        self.cursor = 0;
        self.anchor = None;
        self.undo.clear();
        self.redo.clear();
    }

    /// Replace the whole line, putting the cursor at the end. Used by history
    /// recall and completion acceptance.
    pub fn set(&mut self, text: &str) {
        self.checkpoint();
        self.chars = text.chars().collect();
        self.cursor = self.chars.len();
        self.anchor = None;
    }

    /// Replace the whole line with `text` and place the cursor at char index
    /// `cursor` (clamped). Used to apply a completion built against a saved
    /// original line, and to restore that original on cancel.
    pub fn set_with_cursor(&mut self, text: &str, cursor: usize) {
        self.checkpoint();
        self.chars = text.chars().collect();
        self.cursor = cursor.min(self.chars.len());
        self.anchor = None;
    }
}

#[cfg(test)]
mod tests {
    use super::CmdEditor;

    fn ed(text: &str, cursor: usize) -> CmdEditor {
        let mut e = CmdEditor::new();
        e.insert_str(text);
        e.cursor = cursor;
        e
    }

    #[test]
    fn prepend_str_keeps_caret_and_selection_on_their_chars() {
        // Adopting gap typeahead: the seed was typed chronologically *before*
        // whatever is already in the editor, so it lands at the start while
        // the caret (and any selection) stays on the characters it was on.
        let mut e = ed("etty", 2); // caret between "et" and "ty"
        e.prepend_str("cd g");
        assert_eq!(e.text(), "cd getty");
        assert_eq!(e.cursor(), 6, "caret still between 'et' and 'ty'");
        // One undo removes the adopted seed again.
        e.undo();
        assert_eq!(e.text(), "etty");

        let mut e = ed("tty", 3);
        e.set_cursor(1);
        e.begin_selection();
        e.extend_to(3); // selects "ty"
        e.prepend_str("ge");
        assert_eq!(e.text(), "getty");
        assert_eq!(e.selection(), Some((3, 5)), "selection still covers 'ty'");
    }

    #[test]
    fn insert_and_text() {
        let mut e = CmdEditor::new();
        e.insert_str("git");
        e.insert_str(" ");
        e.insert_str("push");
        assert_eq!(e.text(), "git push");
        assert_eq!(e.cursor(), 8);
    }

    #[test]
    fn insert_mid_line() {
        let mut e = ed("gitpush", 3);
        e.insert_str(" ");
        assert_eq!(e.text(), "git push");
        assert_eq!(e.cursor(), 4);
    }

    #[test]
    fn backspace_and_delete() {
        let mut e = ed("abc", 2);
        e.backspace();
        assert_eq!((e.text().as_str(), e.cursor()), ("ac", 1));
        e.delete();
        assert_eq!((e.text().as_str(), e.cursor()), ("a", 1));
        // Backspace at start is a no-op.
        let mut s = ed("x", 0);
        s.backspace();
        assert_eq!(s.text(), "x");
    }

    #[test]
    fn cursor_motion_bounds() {
        let mut e = ed("ab", 1);
        e.move_left();
        assert_eq!(e.cursor(), 0);
        e.move_left();
        assert_eq!(e.cursor(), 0); // clamped
        e.move_end();
        assert_eq!(e.cursor(), 2);
        e.move_right();
        assert_eq!(e.cursor(), 2); // clamped
        e.move_home();
        assert_eq!(e.cursor(), 0);
    }

    #[test]
    fn word_motion_and_delete() {
        let mut e = ed("git push origin", 15);
        e.move_word_left();
        assert_eq!(e.cursor(), 9); // start of "origin"
        e.move_word_left();
        assert_eq!(e.cursor(), 4); // start of "push"
        let mut d = ed("git push origin", 15);
        d.delete_word_left();
        assert_eq!(d.text(), "git push ");
        assert_eq!(d.cursor(), 9);
    }

    #[test]
    fn delete_to_start_and_end() {
        let mut s = ed("hello world", 6);
        s.delete_to_start();
        assert_eq!((s.text().as_str(), s.cursor()), ("world", 0));
        let mut e = ed("hello world", 5);
        e.delete_to_end();
        assert_eq!((e.text().as_str(), e.cursor()), ("hello", 5));
    }

    #[test]
    fn multibyte_byte_offset() {
        let mut e = CmdEditor::new();
        e.insert_str("你好"); // 2 chars, 6 bytes
        assert_eq!(e.cursor(), 2);
        assert_eq!(e.cursor_byte(), 6);
        e.move_left();
        assert_eq!(e.cursor_byte(), 3); // after first char (3 bytes)
        e.backspace();
        assert_eq!(e.text(), "好");
    }

    #[test]
    fn set_replaces_and_puts_cursor_at_end() {
        let mut e = ed("abc", 1);
        e.set("git status");
        assert_eq!((e.text().as_str(), e.cursor()), ("git status", 10));
    }

    #[test]
    fn set_cursor_clamps() {
        let mut e = ed("hello", 5);
        e.set_cursor(2);
        assert_eq!(e.cursor(), 2);
        e.set_cursor(99);
        assert_eq!(e.cursor(), 5); // clamped to len
    }

    #[test]
    fn selection_basics_and_delete() {
        let mut e = ed("hello world", 0);
        e.begin_selection();
        e.set_cursor(5); // select "hello"
        assert_eq!(e.selection(), Some((0, 5)));
        assert_eq!(e.selected_text().as_deref(), Some("hello"));
        assert!(e.delete_selection());
        assert_eq!((e.text().as_str(), e.cursor()), (" world", 0));
        assert_eq!(e.selection(), None);
    }

    #[test]
    fn typing_replaces_selection() {
        let mut e = ed("abc def", 0);
        e.begin_selection();
        e.set_cursor(3); // select "abc"
        e.insert_str("XY");
        assert_eq!((e.text().as_str(), e.cursor()), ("XY def", 2));
        assert_eq!(e.selection(), None);
    }

    #[test]
    fn select_word_and_all() {
        let mut e = ed("git push origin", 6);
        e.select_word_at(6); // cursor on "push"
        assert_eq!(e.selected_text().as_deref(), Some("push"));
        e.select_all();
        assert_eq!(e.selection(), Some((0, 15)));
    }

    #[test]
    fn extend_to_keeps_anchor() {
        let mut e = ed("abcdef", 2);
        e.extend_to(5);
        assert_eq!(e.selection(), Some((2, 5)));
        e.extend_to(0); // drag back past the anchor
        assert_eq!(e.selection(), Some((0, 2)));
    }

    #[test]
    fn undo_redo_steps_through_edits() {
        let mut e = CmdEditor::new();
        e.insert_str("a");
        e.insert_str("b");
        e.insert_str("c");
        assert_eq!(e.text(), "abc");
        e.undo();
        assert_eq!(e.text(), "ab");
        e.undo();
        assert_eq!(e.text(), "a");
        e.redo();
        assert_eq!(e.text(), "ab");
        // A fresh edit clears the redo stack.
        e.insert_str("X");
        e.redo();
        assert_eq!(e.text(), "abX");
    }

    #[test]
    fn no_op_edit_does_not_swallow_the_first_undo() {
        // A no-op deletion (Backspace with the caret at column 0) used to push a
        // checkpoint equal to the current buffer, so the next Undo was a dead press
        // that "restored" the same text instead of undoing the real edit before it.
        let mut e = ed("x", 0); // buffer "x"; one real edit sits on the undo stack
        e.backspace(); // no-op: nothing before the caret
        e.undo(); // must undo the real insert ("x" -> ""), not the phantom no-op
        assert_eq!(e.text(), "");
    }

    #[test]
    fn no_op_edit_between_real_edits_is_not_a_dead_undo_step() {
        // Same defect via a different no-op path (Ctrl-K at end of line) sitting
        // between two real edits: one Undo must still step back over a real edit.
        let mut e = CmdEditor::new();
        e.insert_str("a");
        e.insert_str("b"); // buffer "ab", caret at end
        e.delete_to_end(); // no-op: the caret is already at the end
        e.undo();
        assert_eq!(e.text(), "a");
    }

    #[test]
    fn undo_restores_the_pre_edit_cursor_position() {
        // A mid-line edit then Undo puts the caret back where the edit began,
        // not at the end of the line.
        let mut e = ed("git push", 3);
        e.insert_str("XY"); // "gitXY push", caret 5
        assert_eq!((e.text().as_str(), e.cursor()), ("gitXY push", 5));
        e.undo();
        assert_eq!((e.text().as_str(), e.cursor()), ("git push", 3));
        e.redo();
        assert_eq!(e.text(), "gitXY push");
    }

    #[test]
    fn select_word_at_snaps_left_from_whitespace_and_clamps() {
        // A double-click on the gap right after a word snaps left and selects
        // that word — the same left-scan that makes a double-click at the end
        // of the line select the last word.
        let mut e = ed("ab cd", 0);
        e.select_word_at(2); // the space between the words
        assert_eq!(e.selected_text().as_deref(), Some("ab"));
        // Index at/past the end selects the trailing word, clamped.
        e.select_word_at(99);
        assert_eq!(e.selected_text().as_deref(), Some("cd"));
        // On a gap wider than one cell there is no adjacent word to the left of
        // the clicked cell: the empty range collapses to no selection.
        let mut e = ed("ab  cd", 0);
        e.select_word_at(3); // second space: both neighbours are whitespace
        assert_eq!(e.selection(), None);
    }

    #[test]
    fn clear_resets_line_cursor_and_undo_history() {
        let mut e = ed("git push", 8);
        e.clear();
        assert!(e.is_empty());
        assert_eq!(e.cursor(), 0);
        // Undo after clear (post-submit) must not resurrect the shipped line.
        e.undo();
        assert!(e.is_empty());
    }

    #[test]
    fn delete_word_right_removes_following_word() {
        let mut e = ed("git push", 0);
        e.delete_word_right();
        assert_eq!(e.text(), " push");
        assert_eq!(e.cursor(), 0);
    }

    #[test]
    fn forward_word_delete_with_selection_leaves_no_out_of_range_slice() {
        // Shift-select " cd" leftward (anchor=5, cursor=2), then Alt+Delete
        // (delete_word_right) drains exactly that region, shrinking "ab cd" -> "ab"
        // but historically leaving the anchor at 5. selected_text() (Cmd+C / Cmd+X)
        // then sliced chars[2..5] on a length-2 Vec and panicked, crashing the app.
        let mut e = ed("ab cd", 5);
        e.extend_to(2);
        assert_eq!(e.selection(), Some((2, 5)));
        e.delete_word_right();
        assert_eq!(e.text(), "ab");
        // RED before the fix: selection() returns Some((2, 5)) and selected_text()
        // panics slicing out of range. GREEN: the stale anchor is clamped away.
        assert_eq!(e.selection(), None);
        assert_eq!(e.selected_text(), None);
    }

    #[test]
    fn mid_buffer_word_delete_shifts_the_anchor_instead_of_faking_a_selection() {
        // Regression: shift-select "def" leftward in "abc def x" (anchor=7,
        // cursor=4), then Alt+Delete removes exactly that word *mid-buffer*.
        // Clamping alone left anchor=7 → clamped to 6 → a phantom (4,6)
        // selection over " x", text the user never selected (and ⌘C copied).
        // Shifting the anchor across the removed range collapses it onto the
        // cursor: no selection survives the deletion of its own text.
        let mut e = ed("abc def x", 7);
        e.extend_to(4);
        assert_eq!(e.selected_text().as_deref(), Some("def"));
        e.delete_word_right();
        assert_eq!(e.text(), "abc  x");
        assert_eq!(e.selection(), None);
        assert_eq!(e.selected_text(), None);
    }

    #[test]
    fn deletions_before_a_selection_keep_it_on_the_same_text() {
        // A range delete strictly before the selection shifts it left as a
        // block, so it keeps covering the same characters.
        let mut e = ed("one two THREE", 13);
        e.extend_to(8); // select "THREE" (anchor=13, cursor=8)
        assert_eq!(e.selected_text().as_deref(), Some("THREE"));
        e.set_cursor(8); // collapse cursor at the selection start… keep anchor
        e.delete_to_start(); // Ctrl+U wipes "one two " before it
        assert_eq!(e.text(), "THREE");
        assert_eq!(e.selected_text().as_deref(), Some("THREE"));
    }

    #[test]
    fn forward_word_delete_preserves_a_selection_it_did_not_touch() {
        // Rightward selection "ab" (anchor=0, cursor=2); Alt+Delete removes the
        // *following* word (" cd"), which doesn't overlap the selection, so the
        // still-valid "ab" selection must survive (clamping is a no-op here).
        let mut e = ed("ab cd", 0);
        e.extend_to(2);
        assert_eq!(e.selection(), Some((0, 2)));
        e.delete_word_right();
        assert_eq!(e.text(), "ab");
        assert_eq!(e.selected_text().as_deref(), Some("ab"));
    }

    #[test]
    fn set_with_cursor_sets_line_and_clamps() {
        let mut e = ed("abc", 1);
        e.set_with_cursor("git status", 3);
        assert_eq!((e.text().as_str(), e.cursor()), ("git status", 3));
        e.set_with_cursor("hi", 99);
        assert_eq!((e.text().as_str(), e.cursor()), ("hi", 2)); // clamped
    }
}
