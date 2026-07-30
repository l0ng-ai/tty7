#[derive(Default)]
pub struct CmdEditor {
    chars: Vec<char>,
    cursor: usize,
    anchor: Option<usize>,
    undo: Vec<(Vec<char>, usize)>,
    redo: Vec<(Vec<char>, usize)>,
    kill: String,
}

const UNDO_LIMIT: usize = 200;

impl CmdEditor {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn text(&self) -> String {
        self.chars.iter().collect()
    }

    pub fn is_empty(&self) -> bool {
        self.chars.is_empty()
    }

    pub fn len(&self) -> usize {
        self.chars.len()
    }

    #[allow(dead_code)]
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    #[allow(dead_code)]
    pub fn cursor_byte(&self) -> usize {
        self.chars[..self.cursor].iter().map(|c| c.len_utf8()).sum()
    }

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

    pub fn insert_str(&mut self, s: &str) {
        self.checkpoint();
        self.delete_selection();
        for c in s.chars() {
            self.chars.insert(self.cursor, c);
            self.cursor += 1;
        }
    }

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

    pub fn delete(&mut self) {
        self.checkpoint();
        if self.delete_selection() {
            return;
        }
        if self.cursor < self.chars.len() {
            self.chars.remove(self.cursor);
        }
    }

    pub fn selection(&self) -> Option<(usize, usize)> {
        let n = self.chars.len();
        let a = self.anchor?.min(n);
        let c = self.cursor.min(n);
        if a == c {
            None
        } else {
            Some((a.min(c), a.max(c)))
        }
    }

    pub fn selected_text(&self) -> Option<String> {
        let (s, e) = self.selection()?;
        Some(self.chars[s..e].iter().collect())
    }

    pub fn clear_selection(&mut self) {
        self.anchor = None;
    }

    pub fn begin_selection(&mut self) {
        if self.anchor.is_none() {
            self.anchor = Some(self.cursor);
        }
    }

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

    pub fn select_all(&mut self) {
        self.anchor = Some(0);
        self.cursor = self.chars.len();
    }

    pub fn word_bounds(&self, idx: usize, separators: &str, smart: bool) -> (usize, usize) {
        let idx = idx.min(self.chars.len());
        if !smart {
            return self.plain_word_bounds(idx, separators, smart);
        }
        if let Some((s, e)) = super::smart_select::pair_range(&self.chars, idx) {
            return (s, e + 1);
        }
        if let Some(&c) = self.chars.get(idx)
            && super::smart_select::is_cjk(c)
        {
            let text: String = self.chars.iter().collect();
            if let Some((s, e)) = super::smart_select::cjk_word_range(&text, idx) {
                return (s, e + 1);
            }
        }
        self.plain_word_bounds(idx, separators, smart)
    }

    fn plain_word_bounds(&self, idx: usize, separators: &str, smart: bool) -> (usize, usize) {
        let idx = idx.min(self.chars.len());
        if let Some(&c) = self.chars.get(idx)
            && !c.is_whitespace()
            && separators.contains(c)
        {
            return (idx, idx + 1);
        }
        let boundary = |c: char| c.is_whitespace() || separators.contains(c);
        let mut s = idx;
        while s > 0 && !boundary(self.chars[s - 1]) {
            s -= 1;
        }
        let mut e = idx;
        while e < self.chars.len() && !boundary(self.chars[e]) {
            e += 1;
        }
        if smart && idx < e {
            let (ns, ne) = super::smart_select::narrow_to_script(&self.chars, idx, s, e - 1);
            return (ns, ne + 1);
        }
        (s, e)
    }

    pub fn select_word_at(&mut self, idx: usize, separators: &str, smart: bool) {
        let (s, e) = self.word_bounds(idx, separators, smart);
        self.anchor = Some(s);
        self.cursor = e;
    }

    pub fn extend_word_to(
        &mut self,
        anchor_start: usize,
        anchor_end: usize,
        idx: usize,
        separators: &str,
        smart: bool,
    ) {
        let (ws, we) = self.plain_word_bounds(idx.min(self.chars.len()), separators, smart);
        if we >= anchor_end {
            self.anchor = Some(anchor_start);
            self.cursor = we;
        } else {
            self.anchor = Some(anchor_end);
            self.cursor = ws;
        }
    }

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

    pub fn line_start(&self, idx: usize) -> usize {
        let mut s = idx.min(self.chars.len());
        while s > 0 && self.chars[s - 1] != '\n' {
            s -= 1;
        }
        s
    }

    pub fn line_end(&self, idx: usize) -> usize {
        let mut e = idx.min(self.chars.len());
        while e < self.chars.len() && self.chars[e] != '\n' {
            e += 1;
        }
        e
    }

    pub fn move_home(&mut self) {
        self.cursor = self.line_start(self.cursor);
    }

    pub fn set_cursor(&mut self, idx: usize) {
        self.cursor = idx.min(self.chars.len());
    }

    pub fn move_end(&mut self) {
        self.cursor = self.line_end(self.cursor);
    }

    pub fn move_word_left(&mut self) {
        while self.cursor > 0 && self.chars[self.cursor - 1].is_whitespace() {
            self.cursor -= 1;
        }
        while self.cursor > 0 && !self.chars[self.cursor - 1].is_whitespace() {
            self.cursor -= 1;
        }
    }

    pub fn move_word_right(&mut self) {
        let n = self.chars.len();
        while self.cursor < n && self.chars[self.cursor].is_whitespace() {
            self.cursor += 1;
        }
        while self.cursor < n && !self.chars[self.cursor].is_whitespace() {
            self.cursor += 1;
        }
    }

    fn shift_anchor_for_removal(&mut self, s: usize, e: usize) {
        if let Some(a) = self.anchor {
            self.anchor = Some(if a <= s { a } else { a.max(e) - (e - s) });
        }
    }

    fn kill_range(&mut self, s: usize, e: usize) {
        self.kill = self.chars[s..e].iter().collect();
        self.chars.drain(s..e);
        self.shift_anchor_for_removal(s, e);
    }

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
        self.kill_range(self.cursor, e);
    }

    pub fn delete_word_left(&mut self) {
        self.checkpoint();
        let end = self.cursor;
        self.move_word_left();
        self.kill_range(self.cursor, end);
    }

    pub fn delete_to_start(&mut self) {
        self.checkpoint();
        let end = self.cursor;
        self.cursor = 0;
        self.kill_range(0, end);
    }

    pub fn delete_to_end(&mut self) {
        self.checkpoint();
        let end = self.chars.len();
        self.kill_range(self.cursor, end);
    }

    pub fn yank(&mut self) {
        if self.kill.is_empty() {
            return;
        }
        let kill = std::mem::take(&mut self.kill);
        self.insert_str(&kill);
        self.kill = kill;
    }

    pub fn clear(&mut self) {
        self.chars.clear();
        self.cursor = 0;
        self.anchor = None;
        self.undo.clear();
        self.redo.clear();
    }

    pub fn set(&mut self, text: &str) {
        self.checkpoint();
        self.chars = text.chars().collect();
        self.cursor = self.chars.len();
        self.anchor = None;
    }

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
        let mut e = ed("etty", 2);
        e.prepend_str("cd g");
        assert_eq!(e.text(), "cd getty");
        assert_eq!(e.cursor(), 6, "caret still between 'et' and 'ty'");
        e.undo();
        assert_eq!(e.text(), "etty");

        let mut e = ed("tty", 3);
        e.set_cursor(1);
        e.begin_selection();
        e.extend_to(3);
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
        assert_eq!(e.cursor(), 0);
        e.move_end();
        assert_eq!(e.cursor(), 2);
        e.move_right();
        assert_eq!(e.cursor(), 2);
        e.move_home();
        assert_eq!(e.cursor(), 0);
    }

    #[test]
    fn word_motion_and_delete() {
        let mut e = ed("git push origin", 15);
        e.move_word_left();
        assert_eq!(e.cursor(), 9);
        e.move_word_left();
        assert_eq!(e.cursor(), 4);
        let mut d = ed("git push origin", 15);
        d.delete_word_left();
        assert_eq!(d.text(), "git push ");
        assert_eq!(d.cursor(), 9);
    }

    #[test]
    fn kills_fill_the_kill_buffer_and_yank_puts_it_back() {
        let mut e = ed("git push origin", 15);
        e.delete_word_left();
        assert_eq!(e.text(), "git push ");
        e.yank();
        assert_eq!((e.text().as_str(), e.cursor()), ("git push origin", 15));

        let mut k = ed("hello world", 5);
        k.delete_to_end();
        assert_eq!(k.text(), "hello");
        k.yank();
        assert_eq!(k.text(), "hello world");

        let mut u = ed("hello world", 6);
        u.delete_to_start();
        assert_eq!(u.text(), "world");
        u.move_end();
        u.yank();
        assert_eq!(u.text(), "worldhello ");

        let mut d = ed("git push origin", 4);
        d.delete_word_right();
        assert_eq!(d.text(), "git  origin");
        d.yank();
        assert_eq!(d.text(), "git push origin");
    }

    #[test]
    fn character_deletes_leave_the_kill_buffer_alone() {
        let mut e = ed("git push origin", 15);
        e.delete_word_left();
        e.backspace();
        e.delete();
        assert_eq!(e.text(), "git push");
        e.yank();
        assert_eq!(e.text(), "git pushorigin");
    }

    #[test]
    fn yank_without_a_kill_does_nothing() {
        let mut e = ed("hello", 3);
        e.yank();
        assert_eq!((e.text().as_str(), e.cursor()), ("hello", 3));
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
        e.insert_str("你好");
        assert_eq!(e.cursor(), 2);
        assert_eq!(e.cursor_byte(), 6);
        e.move_left();
        assert_eq!(e.cursor_byte(), 3);
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
        assert_eq!(e.cursor(), 5);
    }

    #[test]
    fn selection_basics_and_delete() {
        let mut e = ed("hello world", 0);
        e.begin_selection();
        e.set_cursor(5);
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
        e.set_cursor(3);
        e.insert_str("XY");
        assert_eq!((e.text().as_str(), e.cursor()), ("XY def", 2));
        assert_eq!(e.selection(), None);
    }

    const SEPS: &str = ",│`|:\"' ()[]{}<>\t";

    #[test]
    fn select_word_and_all() {
        let mut e = ed("git push origin", 6);
        e.select_word_at(6, SEPS, true);
        assert_eq!(e.selected_text().as_deref(), Some("push"));
        e.select_all();
        assert_eq!(e.selection(), Some((0, 15)));
    }

    #[test]
    fn select_word_stops_at_separators() {
        let mut e = ed("echo 'a,b'", 0);
        e.select_word_at(6, SEPS, true);
        assert_eq!(e.selected_text().as_deref(), Some("a"));
        e.select_word_at(7, SEPS, true);
        assert_eq!(e.selected_text().as_deref(), Some(","));
        e.select_word_at(5, SEPS, true);
        assert_eq!(e.selected_text().as_deref(), Some("'a,b'"));
        let mut e = ed("cat ./a-b/c_d.txt", 0);
        e.select_word_at(8, SEPS, true);
        assert_eq!(e.selected_text().as_deref(), Some("./a-b/c_d.txt"));
    }

    #[test]
    fn extend_to_keeps_anchor() {
        let mut e = ed("abcdef", 2);
        e.extend_to(5);
        assert_eq!(e.selection(), Some((2, 5)));
        e.extend_to(0);
        assert_eq!(e.selection(), Some((0, 2)));
    }

    #[test]
    fn extend_word_to_grows_by_whole_words_both_directions() {
        let mut e = ed("git push origin main", 4);
        e.select_word_at(6, SEPS, true);
        let (s, a) = e.selection().unwrap();
        assert_eq!((s, a), (4, 8));

        e.extend_word_to(s, a, 10, SEPS, true);
        assert_eq!(e.selected_text().as_deref(), Some("push origin"));
        e.extend_word_to(s, a, 18, SEPS, true);
        assert_eq!(e.selected_text().as_deref(), Some("push origin main"));

        e.extend_word_to(s, a, 1, SEPS, true);
        assert_eq!(e.selected_text().as_deref(), Some("git push"));
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
        e.insert_str("X");
        e.redo();
        assert_eq!(e.text(), "abX");
    }

    #[test]
    fn no_op_edit_does_not_swallow_the_first_undo() {
        let mut e = ed("x", 0);
        e.backspace();
        e.undo();
        assert_eq!(e.text(), "");
    }

    #[test]
    fn no_op_edit_between_real_edits_is_not_a_dead_undo_step() {
        let mut e = CmdEditor::new();
        e.insert_str("a");
        e.insert_str("b");
        e.delete_to_end();
        e.undo();
        assert_eq!(e.text(), "a");
    }

    #[test]
    fn undo_restores_the_pre_edit_cursor_position() {
        let mut e = ed("git push", 3);
        e.insert_str("XY");
        assert_eq!((e.text().as_str(), e.cursor()), ("gitXY push", 5));
        e.undo();
        assert_eq!((e.text().as_str(), e.cursor()), ("git push", 3));
        e.redo();
        assert_eq!(e.text(), "gitXY push");
    }

    #[test]
    fn select_word_at_snaps_left_from_whitespace_and_clamps() {
        let mut e = ed("ab cd", 0);
        e.select_word_at(2, SEPS, true);
        assert_eq!(e.selected_text().as_deref(), Some("ab"));
        e.select_word_at(99, SEPS, true);
        assert_eq!(e.selected_text().as_deref(), Some("cd"));
        let mut e = ed("ab  cd", 0);
        e.select_word_at(3, SEPS, true);
        assert_eq!(e.selection(), None);
    }

    #[test]
    fn clear_resets_line_cursor_and_undo_history() {
        let mut e = ed("git push", 8);
        e.clear();
        assert!(e.is_empty());
        assert_eq!(e.cursor(), 0);
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
        let mut e = ed("ab cd", 5);
        e.extend_to(2);
        assert_eq!(e.selection(), Some((2, 5)));
        e.delete_word_right();
        assert_eq!(e.text(), "ab");
        assert_eq!(e.selection(), None);
        assert_eq!(e.selected_text(), None);
    }

    #[test]
    fn mid_buffer_word_delete_shifts_the_anchor_instead_of_faking_a_selection() {
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
        let mut e = ed("one two THREE", 13);
        e.extend_to(8);
        assert_eq!(e.selected_text().as_deref(), Some("THREE"));
        e.set_cursor(8);
        e.delete_to_start();
        assert_eq!(e.text(), "THREE");
        assert_eq!(e.selected_text().as_deref(), Some("THREE"));
    }

    #[test]
    fn forward_word_delete_preserves_a_selection_it_did_not_touch() {
        let mut e = ed("ab cd", 0);
        e.extend_to(2);
        assert_eq!(e.selection(), Some((0, 2)));
        e.delete_word_right();
        assert_eq!(e.text(), "ab");
        assert_eq!(e.selected_text().as_deref(), Some("ab"));
    }

    #[test]
    fn home_end_are_logical_line_relative_in_a_multiline_buffer() {
        let mut e = ed("one\ntwo\nthree", 5);
        e.move_home();
        assert_eq!(e.cursor(), 4, "start of the 'two' line");
        e.move_end();
        assert_eq!(e.cursor(), 7, "end of the 'two' line (before the '\\n')");
        e.set_cursor(1);
        e.move_home();
        assert_eq!(e.cursor(), 0);
        e.move_end();
        assert_eq!(e.cursor(), 3);
        e.set_cursor(10);
        e.move_end();
        assert_eq!(e.cursor(), 13);
        let mut s = ed("git push", 4);
        s.move_home();
        assert_eq!(s.cursor(), 0);
        s.move_end();
        assert_eq!(s.cursor(), 8);
    }

    #[test]
    fn set_with_cursor_sets_line_and_clamps() {
        let mut e = ed("abc", 1);
        e.set_with_cursor("git status", 3);
        assert_eq!((e.text().as_str(), e.cursor()), ("git status", 3));
        e.set_with_cursor("hi", 99);
        assert_eq!((e.text().as_str(), e.cursor()), ("hi", 2));
    }
}
