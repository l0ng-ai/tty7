const RECORD_CAP: usize = 4096;

#[derive(Default)]
pub struct Typeahead {
    text: String,
    tainted: bool,
    pasted: bool,
}

pub enum RawInput<'a> {
    Text(&'a str),
    /// [`Text`](Self::Text) for text that came off the clipboard rather than
    /// the keyboard. The record replays into the editor's buffer, so the
    /// provenance has to survive the round trip or a paste comes back looking
    /// typed and is submitted as typed (#660).
    Pasted(&'a str),
    Key {
        key: &'a str,
        plain: bool,
    },
    Interrupt,
}

impl Typeahead {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn observe(&mut self, input: RawInput, externally_owned: bool) {
        match input {
            RawInput::Interrupt => self.discard(),
            _ if externally_owned => {}
            RawInput::Text(s) => self.record_text(s),
            RawInput::Pasted(s) => {
                self.record_text(s);
                self.pasted = true;
            }
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

    /// Discard a record at an input-ownership boundary without producing a
    /// shell-line wipe.
    pub fn discard(&mut self) {
        *self = Self::default();
    }

    /// Whether any of the recorded text came off the clipboard. Read it before
    /// [`drain`](Self::drain) or [`adopt`](Self::adopt), which clear it with
    /// the seed — the same contract `GapHold::pasted` carries.
    pub fn pasted(&self) -> bool {
        self.pasted
    }

    pub fn drain(&mut self) -> Option<String> {
        std::mem::take(self).flush()
    }

    /// Hand the seed to the local editor while leaving the wipe owed.
    ///
    /// The shell is still sitting on this text, so the `^U` that erases it has
    /// to go out eventually — but not necessarily now. Taking the seed out and
    /// keeping the record in its tainted (wipe, seed nothing) shape lets the
    /// editor own the whole line straight away, and the next `drain` still
    /// produces the wipe.
    pub fn adopt(&mut self) -> Option<String> {
        let seed = self.drain()?;
        self.tainted = true;
        Some(seed)
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
        if self.text.is_empty() {
            if self.tainted {
                return Some(String::new());
            }
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
    fn alt_screen_input_is_discarded_without_a_shell_gap() {
        let mut t = Typeahead::new();
        t.observe(RawInput::Text("q"), true);
        assert_eq!(t.drain(), None);
    }

    #[test]
    fn alt_screen_input_does_not_taint_later_shell_input() {
        let mut t = Typeahead::new();
        t.observe(RawInput::Text("q"), true);
        t.observe(RawInput::Text("ls"), false);
        assert_eq!(t.drain(), Some("ls".to_string()));
    }

    #[test]
    fn interrupt_discards_a_gap_even_without_alt_screen() {
        let mut t = Typeahead::new();
        t.observe(RawInput::Text("agent input"), false);
        t.observe(
            RawInput::Key {
                key: "up",
                plain: true,
            },
            false,
        );

        t.observe(RawInput::Interrupt, false);
        assert_eq!(t.drain(), None);
    }

    #[test]
    fn discarding_a_tainted_record_starts_a_fresh_gap_without_a_shell_wipe() {
        let mut t = Typeahead::new();
        t.observe(RawInput::Text("ls"), false);
        t.observe(
            RawInput::Key {
                key: "up",
                plain: true,
            },
            false,
        );
        t.discard();
        assert_eq!(t.drain(), None);

        t.observe(RawInput::Text("git status"), false);
        assert_eq!(t.drain(), Some("git status".to_string()));
    }

    #[test]
    fn untouched_record_flushes_to_none() {
        assert_eq!(Typeahead::new().drain(), None);
    }

    #[test]
    fn adopting_moves_the_seed_out_and_leaves_the_wipe_owed() {
        let mut t = Typeahead::new();
        t.observe(RawInput::Text("echo"), false);
        assert_eq!(t.adopt(), Some("echo".to_string()));
        // The seed is the editor's now, but the shell is still holding it.
        assert_eq!(t.drain(), Some(String::new()));
        assert_eq!(t.drain(), None);
    }

    #[test]
    fn adopting_an_empty_record_owes_nothing() {
        let mut t = Typeahead::new();
        assert_eq!(t.adopt(), None);
        assert_eq!(t.drain(), None);
    }

    #[test]
    fn adopting_twice_seeds_once() {
        let mut t = Typeahead::new();
        t.observe(RawInput::Text("echo"), false);
        assert_eq!(t.adopt(), Some("echo".to_string()));
        assert_eq!(t.adopt(), Some(String::new()));
        assert_eq!(t.drain(), Some(String::new()));
    }

    #[test]
    fn a_pasted_gap_replays_as_a_paste() {
        let mut t = Typeahead::new();
        assert!(!t.pasted(), "a fresh record carries nothing pasted");
        t.observe(RawInput::Text("cat "), false);
        assert!(!t.pasted());
        t.observe(RawInput::Pasted("/tmp/x"), false);
        assert!(t.pasted(), "the whole seed is pasted once any of it is");
        assert_eq!(t.drain(), Some("cat /tmp/x".to_string()));
        assert!(!t.pasted(), "the drain hands the mark over with the seed");
    }

    #[test]
    fn adopting_hands_the_paste_mark_over_with_the_seed() {
        let mut t = Typeahead::new();
        t.observe(RawInput::Pasted("ls"), false);
        assert_eq!(t.adopt(), Some("ls".to_string()));
        assert!(!t.pasted(), "what is left is the owed wipe, not a paste");
        assert_eq!(t.drain(), Some(String::new()));
    }

    #[test]
    fn discarding_an_adopted_record_drops_the_owed_wipe() {
        let mut t = Typeahead::new();
        t.observe(RawInput::Text("echo"), false);
        assert_eq!(t.adopt(), Some("echo".to_string()));
        t.discard();
        assert_eq!(t.drain(), None);
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
