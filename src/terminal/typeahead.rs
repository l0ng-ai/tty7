const RECORD_CAP: usize = 4096;

#[derive(Default)]
pub struct Typeahead {
    text: String,
    tainted: bool,
}

pub enum RawInput<'a> {
    Text(&'a str),
    Key { key: &'a str, plain: bool },
}

impl Typeahead {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn observe(&mut self, input: RawInput, alt_screen: bool) {
        if alt_screen {
            self.taint();
            return;
        }
        match input {
            RawInput::Text(s) => self.record_text(s),
            RawInput::Key {
                key: "enter",
                plain: true,
            } => self.record_enter(),
            RawInput::Key {
                key: "backspace",
                plain: true,
            } => self.record_backspace(),
            RawInput::Key { .. } => self.taint(),
        }
    }

    pub fn drain(&mut self) -> Option<String> {
        std::mem::take(self).flush()
    }

    fn record_text(&mut self, s: &str) {
        if s.chars().any(char::is_control) {
            self.tainted = true;
            return;
        }
        if self.text.len() + s.len() > RECORD_CAP {
            self.tainted = true;
            return;
        }
        self.text.push_str(s);
    }

    fn record_enter(&mut self) {
        if self.text.len() + 1 > RECORD_CAP {
            self.tainted = true;
            return;
        }
        self.text.push('\r');
    }

    fn record_backspace(&mut self) {
        if !self.text.ends_with('\r') {
            self.text.pop();
        }
    }

    fn taint(&mut self) {
        self.tainted = true;
    }

    fn flush(self) -> Option<String> {
        if self.text.is_empty() && !self.tainted {
            return None;
        }
        if self.tainted {
            return Some(String::new());
        }
        let seed = self.text.rsplit('\r').next().unwrap_or("");
        Some(seed.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drained_record_reconstructs_each_gap_independently() {
        let mut t = Typeahead::new();
        t.observe(RawInput::Text("cd getty"), false);
        assert_eq!(t.drain(), Some("cd getty".to_string()));
        assert_eq!(t.drain(), None);
        t.observe(RawInput::Text("ls"), false);
        assert_eq!(t.drain(), Some("ls".to_string()));
    }

    #[test]
    fn raw_keys_map_to_boundary_erase_or_taint() {
        let mut t = Typeahead::new();
        t.observe(RawInput::Text("ls"), false);
        t.observe(
            RawInput::Key {
                key: "enter",
                plain: true,
            },
            false,
        );
        t.observe(RawInput::Text("git st"), false);
        t.observe(
            RawInput::Key {
                key: "backspace",
                plain: true,
            },
            false,
        );
        assert_eq!(t.drain(), Some("git s".to_string()));

        let mut t = Typeahead::new();
        t.observe(RawInput::Text("ls"), false);
        t.observe(
            RawInput::Key {
                key: "up",
                plain: true,
            },
            false,
        );
        assert_eq!(t.drain(), Some(String::new()));

        let mut t = Typeahead::new();
        t.observe(RawInput::Text("a"), false);
        t.observe(
            RawInput::Key {
                key: "enter",
                plain: false,
            },
            false,
        );
        assert_eq!(t.drain(), Some(String::new()));
    }

    #[test]
    fn alt_screen_input_taints_instead_of_seeding() {
        let mut t = Typeahead::new();
        t.observe(RawInput::Text("q"), true);
        assert_eq!(t.drain(), Some(String::new()));
    }

    #[test]
    fn untouched_record_flushes_to_none() {
        assert_eq!(Typeahead::new().drain(), None);
    }

    #[test]
    fn typed_text_is_wiped_and_seeded() {
        let mut p = Typeahead::new();
        p.record_text("git sta");
        assert_eq!(p.drain(), Some("git sta".to_string()));
    }

    #[test]
    fn backspace_edits_the_record() {
        let mut p = Typeahead::new();
        p.record_text("lsx");
        p.record_backspace();
        assert_eq!(p.drain(), Some("ls".to_string()));
    }

    #[test]
    fn backspace_on_empty_record_is_noop_but_still_flushes_nothing() {
        let mut p = Typeahead::new();
        p.record_backspace();
        assert_eq!(p.drain(), None);
    }

    #[test]
    fn enter_marks_a_submit_boundary() {
        let mut p = Typeahead::new();
        p.record_text("ls");
        p.record_enter();
        p.record_text("git sta");
        assert_eq!(p.drain(), Some("git sta".to_string()));
    }

    #[test]
    fn fully_submitted_input_wipes_but_seeds_nothing() {
        let mut p = Typeahead::new();
        p.record_text("ls");
        p.record_enter();
        assert_eq!(p.drain(), Some(String::new()));
    }

    #[test]
    fn backspace_does_not_cross_a_submit_boundary() {
        let mut p = Typeahead::new();
        p.record_text("ls");
        p.record_enter();
        p.record_backspace();
        assert_eq!(p.drain(), Some(String::new()));
    }

    #[test]
    fn unreconstructable_input_taints_wipe_without_seed() {
        let mut p = Typeahead::new();
        p.record_text("ls");
        p.taint();
        assert_eq!(p.drain(), Some(String::new()));
    }

    #[test]
    fn control_chars_in_committed_text_taint() {
        let mut p = Typeahead::new();
        p.record_text("echo a\necho b");
        assert_eq!(p.drain(), Some(String::new()));
    }

    #[test]
    fn overflowing_the_cap_taints_instead_of_truncating() {
        let mut p = Typeahead::new();
        let chunk = "x".repeat(1000);
        for _ in 0..5 {
            p.record_text(&chunk);
        }
        assert_eq!(p.drain(), Some(String::new()));
    }

    #[test]
    fn exactly_at_the_cap_still_reconstructs() {
        let mut p = Typeahead::new();
        let full = "x".repeat(RECORD_CAP);
        p.record_text(&full);
        assert_eq!(p.drain(), Some(full.clone()));

        let mut p = Typeahead::new();
        p.record_text(&full);
        p.record_text("y");
        assert_eq!(p.drain(), Some(String::new()));
    }

    #[test]
    fn taint_survives_later_clean_typing() {
        let mut p = Typeahead::new();
        p.taint();
        p.record_text("ls");
        p.record_enter();
        assert_eq!(p.drain(), Some(String::new()));
    }
}
