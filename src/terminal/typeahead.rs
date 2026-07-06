//! Tracks what the user types into a pane while the local line editor is
//! *disengaged*, so the editor can adopt it on engage instead of stranding it.
//!
//! Two windows behave identically: a freshly spawned shell sourcing rc files
//! (often a second or more before the first OSC 133), and the gap every
//! submitted command opens between `133;C` and the next prompt. In both,
//! `at_prompt` is false and keystrokes go raw to the PTY. The shell isn't
//! reading them — the bytes queue in the kernel TTY buffer — and when zle
//! (re)starts at the next prompt it consumes them as type-ahead: they appear
//! on the *shell's* command line. At that same moment the editor engages with
//! an empty buffer and swallows every key, so the strays can be neither
//! edited nor deleted — and the editor overlay (transparent, anchored at the
//! cursor) double-draws its own line over their echo.
//!
//! The fix: record a best-effort reconstruction of the gap typing here; when
//! the editor engages, send one `^U` (kill-whole-line) to the PTY and seed
//! the editor with the reconstruction. Ordering makes the `^U` safe with no
//! timing assumptions: it is written *after* every stray byte, and the TTY
//! queue is FIFO, so zle always consumes the strays first and then the `^U`
//! that wipes them — wherever prompt boundaries fall. In the common case
//! nothing was typed in the gap, `drain` returns `None`, and no byte is sent.
//!
//! A command that *reads* its stdin (a REPL, a password prompt) consumes gap
//! bytes itself; they never reach zle. The `^U` still only lands in zle (the
//! editor engages at a prompt, after the command exited), where killing an
//! empty line is a no-op, and Enter-terminated input seeds nothing thanks to
//! the submit-boundary rule — so the wipe stays safe there too. Full-screen
//! TUI input (alt screen) is not reconstructable typing at all: it taints the
//! record instead of recording.

/// Cap on the recorded reconstruction. Typing that overflows it (nobody types
/// 4 KiB into a prompt gap — this is a paste or a stuck key) taints the
/// record instead of silently truncating to a wrong line.
const RECORD_CAP: usize = 4096;

/// Best-effort reconstruction of user input sent raw to the PTY while the
/// line editor was disengaged. `tainted` means bytes we can't reconstruct
/// (arrows, tab, control chords, multi-line pastes) went through: the wipe
/// still happens, but nothing is seeded — a wrong guess in the editor is
/// worse than an empty line.
#[derive(Default)]
pub struct Typeahead {
    text: String,
    tainted: bool,
}

/// One raw PTY-bound user-input event, as far as reconstruction cares.
pub enum RawInput<'a> {
    /// Committed printable text (the IME commit and paste paths).
    Text(&'a str),
    /// A non-text keystroke that produced PTY bytes. `key` is the GPUI key
    /// name; `plain` means no control/alt/platform modifier was held.
    Key { key: &'a str, plain: bool },
}

impl Typeahead {
    pub fn new() -> Self {
        Self::default()
    }

    /// Fold one raw PTY-bound input event into the reconstruction. Call this
    /// wherever user input is written to the PTY while the line editor is
    /// disengaged — the record mirrors exactly what the shell will later
    /// consume as type-ahead. `alt_screen` input belongs to a full-screen TUI,
    /// not the shell's next line: it taints the record instead of recording.
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

    /// Take the reconstruction accumulated since the last drain, resetting the
    /// record so the next gap starts clean. `None` → nothing was typed, send
    /// nothing. `Some(seed)` → send `^U` to wipe the shell's line, then put
    /// `seed` (possibly empty, if tainted or everything was already submitted)
    /// into the editor.
    pub fn drain(&mut self) -> Option<String> {
        std::mem::take(self).flush()
    }

    /// Record committed printable text (the IME path). Control characters mean
    /// this wasn't plain typing (e.g. a multi-line paste) — taint instead.
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

    /// Record Enter. `\r` marks a submit boundary: everything before it will
    /// have been accepted (and run) by zle, so only the tail after the *last*
    /// `\r` is still sitting on the line when we flush.
    fn record_enter(&mut self) {
        if self.text.len() + 1 > RECORD_CAP {
            self.tainted = true;
            return;
        }
        self.text.push('\r');
    }

    /// Record Backspace. Pops the last recorded char — except across a submit
    /// boundary (or on an empty record), where zle itself would have had
    /// nothing to erase, so the record must not shrink either.
    fn record_backspace(&mut self) {
        if !self.text.ends_with('\r') {
            self.text.pop();
        }
    }

    /// Record a byte sequence we can't reconstruct (arrows, tab, control
    /// chords…). The eventual wipe neutralizes whatever zle makes of it; we
    /// just stop pretending to know the line's content.
    fn taint(&mut self) {
        self.tainted = true;
    }

    /// Consume the record into the seed decision — the by-value core of
    /// [`Typeahead::drain`], see there for the contract.
    fn flush(self) -> Option<String> {
        if self.text.is_empty() && !self.tainted {
            return None;
        }
        if self.tainted {
            return Some(String::new());
        }
        // Only the tail after the last submit boundary is still on zle's line.
        let seed = self.text.rsplit('\r').next().unwrap_or("");
        Some(seed.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drained_record_reconstructs_each_gap_independently() {
        // Mid-session, every submitted command opens a prompt→prompt gap where
        // typing goes raw to the PTY. The record must reset on `drain` so each
        // gap seeds only its own typing.
        let mut t = Typeahead::new();
        t.observe(RawInput::Text("cd getty"), false);
        assert_eq!(t.drain(), Some("cd getty".to_string()));
        // The idle prompt drains once per render — nothing typed since, so
        // nothing is wiped or seeded.
        assert_eq!(t.drain(), None);
        // The next gap starts clean, unpolluted by the drained one.
        t.observe(RawInput::Text("ls"), false);
        assert_eq!(t.drain(), Some("ls".to_string()));
    }

    #[test]
    fn raw_keys_map_to_boundary_erase_or_taint() {
        // Plain Enter is a submit boundary (zle ran what precedes it); plain
        // Backspace erases the last recorded char, exactly like zle will.
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

        // Any other key that produced PTY bytes (arrows, tab, chords) makes
        // the line unknowable — wipe, seed nothing.
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

        // A chorded Enter isn't accept-line; it must not fake a boundary.
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
        // Keys typed into a full-screen TUI (vim, less…) are that program's
        // input, not command typing — resurrecting them as an editor seed
        // would turn a habitual `q` into a pending command. They make the
        // line unknowable: wipe at the next prompt, seed nothing.
        let mut t = Typeahead::new();
        t.observe(RawInput::Text("q"), true);
        assert_eq!(t.drain(), Some(String::new()));
    }

    #[test]
    fn untouched_record_flushes_to_none() {
        // The overwhelmingly common case — nothing typed during startup — must
        // send nothing: no ^U, no seed, zero behavior change.
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
        // zle would have nothing to erase either; the record stays empty and
        // the flush stays silent.
        let mut p = Typeahead::new();
        p.record_backspace();
        assert_eq!(p.drain(), None);
    }

    #[test]
    fn enter_marks_a_submit_boundary() {
        // "ls\r" was accepted and executed by zle at the first prompt; nothing
        // of it remains on the line. Seeding "ls" again would duplicate the
        // command — the seed must be only the tail after the last \r.
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
        // ^U still goes out (an empty next line is wiped harmlessly; a partial
        // leak is cleaned), but the executed command is not resurrected.
        assert_eq!(p.drain(), Some(String::new()));
    }

    #[test]
    fn backspace_does_not_cross_a_submit_boundary() {
        // After "ls\r", zle's next line is empty: a Backspace typed then erases
        // nothing in the shell, so it must not eat our \r marker either —
        // otherwise the seed would become "ls" and duplicate the executed command.
        let mut p = Typeahead::new();
        p.record_text("ls");
        p.record_enter();
        p.record_backspace();
        assert_eq!(p.drain(), Some(String::new()));
    }

    #[test]
    fn unreconstructable_input_taints_wipe_without_seed() {
        // An arrow key (history recall!) makes the line's real content
        // unknowable. Wipe it, seed nothing.
        let mut p = Typeahead::new();
        p.record_text("ls");
        p.taint();
        assert_eq!(p.drain(), Some(String::new()));
    }

    #[test]
    fn control_chars_in_committed_text_taint() {
        // A multi-line paste reaches the raw path as one commit; its embedded
        // newlines already ran as commands zle-side. Don't guess.
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
        // 5000 > RECORD_CAP: a truncated seed would be a wrong line; taint.
        assert_eq!(p.drain(), Some(String::new()));
    }

    #[test]
    fn exactly_at_the_cap_still_reconstructs() {
        // Filling the record to exactly RECORD_CAP is not an overflow; the full
        // reconstruction survives. One more char would tip it into taint.
        let mut p = Typeahead::new();
        let full = "x".repeat(RECORD_CAP);
        p.record_text(&full);
        assert_eq!(p.drain(), Some(full.clone()));

        let mut p = Typeahead::new();
        p.record_text(&full);
        p.record_text("y"); // cap + 1 → taint (wipe, no seed)
        assert_eq!(p.drain(), Some(String::new()));
    }

    #[test]
    fn taint_survives_later_clean_typing() {
        // Once the record is unknowable it stays unknowable — later reconstructable
        // keys must not "wash" the taint into a half-right seed.
        let mut p = Typeahead::new();
        p.taint();
        p.record_text("ls");
        p.record_enter();
        assert_eq!(p.drain(), Some(String::new()));
    }
}
