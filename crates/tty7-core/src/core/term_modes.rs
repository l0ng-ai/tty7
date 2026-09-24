//! Tracks the DEC private modes a pane's output has switched on, so a
//! re-attaching client can be told about them instead of having to find them
//! in the replayed bytes.
//!
//! A pane's screen comes back on re-attach as raw bytes out of the replay ring,
//! and the ring is a *window*: it holds the last few megabytes and drops the
//! rest from the front. That is fine for text, which is only worth what is
//! still on screen, and wrong for modes, which a full-screen program sets
//! exactly once — `btop` sends `?1049h` and its mouse modes at startup and then
//! never again, so a day of refreshes pushes the only copy of them out of the
//! ring. The client that replays what is left ends up painting an alternate
//! screen onto its primary buffer with mouse reporting off, and its wheel falls
//! back to scrolling the scrollback of a screen that should not scroll (#774).
//!
//! So the daemon folds the same bytes into this tracker as they pass, and
//! `replay_state` re-sends what is still on ahead of the ring — the same
//! treatment cwd, the prompt state and the agent already get. Only what the
//! ring itself no longer carries, though: see [`TerminalModes::restore_bytes_beyond`].
//!
//! Only modes that change how input is routed or which buffer is on screen are
//! tracked. Cursor visibility (`?25`) and autowrap (`?7`) are deliberately left
//! out: any frame of a running TUI paints them back within milliseconds, while
//! restoring them from a stale fold could leave a shell with an invisible
//! cursor, which is a worse failure than the one being fixed.

/// The modes worth restoring.
///
/// `47`, `1047` and `1049` are the alternate screen in its three spellings —
/// the mode the wheel consults before it decides the pane has a scrollback to
/// move at all, and the one that decides which buffer the replayed frames are
/// painted into. `1000`, `1002` and `1003` are the mouse reporting level and
/// `1005`, `1006`, `1015` and `1016` its encodings: a program that negotiated
/// SGR and comes back without it reads every wheel report as a click at a
/// wrong, truncated coordinate. `1007` is alternate scroll, which is what turns
/// the wheel into arrow keys inside a full-screen program, and `1` (DECCKM)
/// decides whether those arrows are `ESC O A` or `ESC [ A`. `1004` is focus
/// reporting, which the client stops sending without it, and `2004` bracketed
/// paste — without it a paste into a restored TUI arrives as plain keystrokes,
/// which is how a paste turns into commands.
const TRACKED: &[u16] = &[
    1, 47, 1047, 1049, 1000, 1002, 1003, 1004, 1005, 1006, 1007, 1015, 1016, 2004,
];

/// Bracketed paste: whether a paste may arrive framed as `ESC[200~ … ESC[201~`.
pub const BRACKETED_PASTE: u16 = 2004;

/// A CSI longer than this is not a mode set; keep the buffer bounded.
const MAX_PARAMS: usize = 64;

/// The private modes currently on, in the order they were last switched on.
///
/// Order matters because the emulator treats some of these as levels rather
/// than as independent bits: setting `?1002` clears the other mouse-reporting
/// modes. Replaying them in the order the application set them therefore lands
/// on the same state the application asked for, whatever it asked for.
#[derive(Debug, Default, Clone)]
pub struct TerminalModes {
    on: Vec<u16>,
    state: State,
    params: Vec<u8>,
}

#[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    #[default]
    Text,
    Esc,
    /// A CSI whose parameter bytes are being read. `private` records the `?`
    /// that makes it a DEC private mode rather than an ANSI one.
    Csi {
        private: bool,
    },
    Osc,
    OscEsc,
}

impl TerminalModes {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.on.is_empty()
    }

    /// The modes currently on, oldest set first.
    pub fn active(&self) -> &[u16] {
        &self.on
    }

    /// Whether `mode` is on. Only ever true for a mode in [`TRACKED`].
    pub fn is_on(&self, mode: u16) -> bool {
        self.on.contains(&mode)
    }

    /// The bytes that put a freshly reset terminal back into these modes, or
    /// `None` when there is nothing to restore.
    pub fn restore_bytes(&self) -> Option<Vec<u8>> {
        Self::bytes_for(&self.on)
    }

    /// The same, minus every mode `replayed` switches on by itself.
    ///
    /// `replayed` is a fold over the bytes that are about to be sent after
    /// these, and whatever it carries has to be left to it — a mode sequence in
    /// a stream does more than set a bit. `?1049h` clears the alternate screen
    /// and takes the cursor there, and the emulator makes it a *no-op* once the
    /// mode is already on, so restoring such a mode ahead of a replay that
    /// still contains it does not harmlessly double up: it paints everything
    /// the replay wrote before its own `?1049h` into the alternate screen,
    /// which has no scrollback to hold it, and leaves the primary buffer the
    /// program's exit returns the client to empty.
    ///
    /// What is left over is exactly what the replay can no longer speak for,
    /// and it is a prefix of `on`: the replay is a suffix of the stream, so any
    /// mode it sets was set later than one it does not.
    pub fn restore_bytes_beyond(&self, replayed: &TerminalModes) -> Option<Vec<u8>> {
        let missing: Vec<u16> = self
            .on
            .iter()
            .copied()
            .filter(|mode| !replayed.on.contains(mode))
            .collect();
        Self::bytes_for(&missing)
    }

    fn bytes_for(modes: &[u16]) -> Option<Vec<u8>> {
        if modes.is_empty() {
            return None;
        }
        let mut out = Vec::with_capacity(modes.len() * 8);
        for mode in modes {
            out.extend_from_slice(b"\x1b[?");
            out.extend_from_slice(mode.to_string().as_bytes());
            out.push(b'h');
        }
        Some(out)
    }

    /// Folds one chunk of pty output into the tracked state. Sequences split
    /// across chunks are carried, so the caller may feed whatever sizes the pty
    /// hands it.
    pub fn feed(&mut self, bytes: &[u8]) {
        let mut i = 0;
        while i < bytes.len() {
            if self.state == State::Text {
                let Some(off) = memchr::memchr(0x1b, &bytes[i..]) else {
                    return;
                };
                self.state = State::Esc;
                i += off + 1;
                continue;
            }
            let b = bytes[i];
            match self.state {
                State::Text => unreachable!(),
                State::Esc => match b {
                    b'[' => {
                        self.params.clear();
                        self.state = State::Csi { private: false };
                    }
                    b']' => self.state = State::Osc,
                    // RIS. Everything this tracker knows goes back to default,
                    // exactly as it does in the client's emulator.
                    b'c' => {
                        self.on.clear();
                        self.state = State::Text;
                    }
                    0x1b => {}
                    _ => self.state = State::Text,
                },
                State::Csi { private } => match b {
                    b'?' if self.params.is_empty() => self.state = State::Csi { private: true },
                    b'0'..=b'9' | b';' => {
                        self.params.push(b);
                        if self.params.len() > MAX_PARAMS {
                            self.state = State::Text;
                        }
                    }
                    b'h' | b'l' => {
                        if private {
                            self.apply(b == b'h');
                        }
                        self.state = State::Text;
                    }
                    // Any other final byte — or an intermediate such as the `$`
                    // of a DECRQM query — ends a sequence that is not a mode
                    // set. Intermediates are lumped in with finals on purpose:
                    // `?…$p` is a *request*, and answering it is the emulator's
                    // job, not ours.
                    _ => self.state = State::Text,
                },
                State::Osc => match b {
                    0x07 => self.state = State::Text,
                    0x1b => self.state = State::OscEsc,
                    _ => {}
                },
                State::OscEsc => match b {
                    b'\\' => self.state = State::Text,
                    0x1b => {}
                    _ => self.state = State::Osc,
                },
            }
            i += 1;
        }
    }

    fn apply(&mut self, on: bool) {
        for param in self.params.split(|b| *b == b';') {
            let Ok(text) = std::str::from_utf8(param) else {
                continue;
            };
            let Ok(mode) = text.parse::<u16>() else {
                continue;
            };
            if !TRACKED.contains(&mode) {
                continue;
            }
            // Removed either way: switching a mode on again moves it to the
            // back, so the replay repeats the application's own order.
            self.on.retain(|m| *m != mode);
            if on {
                self.on.push(mode);
            }
        }
        self.params.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracks_the_alternate_screen_and_mouse_modes_a_full_screen_tool_sets() {
        let mut modes = TerminalModes::new();
        modes.feed(b"\x1b[?1049h\x1b[?1002h\x1b[?1006h");
        assert_eq!(modes.active(), &[1049, 1002, 1006]);
        assert_eq!(
            modes.restore_bytes().unwrap(),
            b"\x1b[?1049h\x1b[?1002h\x1b[?1006h".to_vec()
        );
    }

    #[test]
    fn a_mode_switched_off_is_forgotten() {
        let mut modes = TerminalModes::new();
        modes.feed(b"\x1b[?1049h\x1b[?1006h");
        modes.feed(b"\x1b[?1049l");
        assert_eq!(modes.active(), &[1006]);

        modes.feed(b"\x1b[?1006l");
        assert!(modes.is_empty());
        assert!(modes.restore_bytes().is_none());
    }

    #[test]
    fn one_csi_may_carry_several_modes() {
        let mut modes = TerminalModes::new();
        modes.feed(b"\x1b[?1000;1002;1006h");
        assert_eq!(modes.active(), &[1000, 1002, 1006]);
        modes.feed(b"\x1b[?1000;1002l");
        assert_eq!(modes.active(), &[1006]);
    }

    #[test]
    fn re_setting_a_mode_moves_it_behind_the_ones_set_since() {
        let mut modes = TerminalModes::new();
        // The emulator treats the reporting modes as a level, so the last one
        // set is the one that wins — the replay has to end on it too.
        modes.feed(b"\x1b[?1002h\x1b[?1003h\x1b[?1002h");
        assert_eq!(modes.active(), &[1003, 1002]);
    }

    #[test]
    fn a_sequence_split_across_chunks_is_still_seen() {
        let mut modes = TerminalModes::new();
        modes.feed(b"\x1b[?10");
        modes.feed(b"49");
        modes.feed(b"h");
        assert_eq!(modes.active(), &[1049]);
    }

    #[test]
    fn untracked_modes_and_ansi_mode_sets_are_ignored() {
        let mut modes = TerminalModes::new();
        // `?25` (cursor) and `?2026` (synchronised update) are not restored,
        // and `[4h` is ANSI insert mode, not a private one.
        modes.feed(b"\x1b[?25l\x1b[?2026h\x1b[4h\x1b[?1049h");
        assert_eq!(modes.active(), &[1049]);
    }

    #[test]
    fn a_mode_query_is_not_a_mode_set() {
        let mut modes = TerminalModes::new();
        modes.feed(b"\x1b[?1049$p");
        assert!(modes.is_empty());
    }

    #[test]
    fn an_osc_payload_that_looks_like_a_mode_set_is_not_one() {
        let mut modes = TerminalModes::new();
        modes.feed(b"\x1b]0;\x1b[?1049h\x07\x1b[?1002h");
        assert_eq!(modes.active(), &[1002]);
    }

    #[test]
    fn modes_the_replay_still_carries_are_left_to_the_replay() {
        let mut modes = TerminalModes::new();
        modes.feed(b"\x1b[?1049h\x1b[?1002h\x1b[?1006h");

        // A ring that still holds the whole prefix speaks for all three, and
        // has to: its own `?1049h` is what clears the alternate screen and
        // decides which buffer the bytes around it are painted into.
        let mut whole = TerminalModes::new();
        whole.feed(b"\x1b[?1049h\x1b[?1002h\x1b[?1006h");
        assert!(modes.restore_bytes_beyond(&whole).is_none());

        // One that holds only the tail speaks for the tail; the rest comes back
        // ahead of it, in the order the application set it.
        let mut tail = TerminalModes::new();
        tail.feed(b"\x1b[?1006h");
        assert_eq!(
            modes.restore_bytes_beyond(&tail).unwrap(),
            b"\x1b[?1049h\x1b[?1002h".to_vec()
        );

        // And a ring with nothing left of the prefix is the #774 case: all of
        // it is re-sent.
        assert_eq!(
            modes.restore_bytes_beyond(&TerminalModes::new()).unwrap(),
            modes.restore_bytes().unwrap()
        );
    }

    /// A ring drops from its front mid-sequence, so its first bytes can be the
    /// tail of a mode set. The emulator will not act on that, so neither does
    /// the fold that stands in for it.
    #[test]
    fn a_mode_set_the_replay_only_half_carries_is_still_restored() {
        let mut modes = TerminalModes::new();
        modes.feed(b"\x1b[?1049h");

        let mut replayed = TerminalModes::new();
        replayed.feed(b"049h and the rest of the screen");
        assert_eq!(
            modes.restore_bytes_beyond(&replayed).unwrap(),
            b"\x1b[?1049h".to_vec()
        );
    }

    #[test]
    fn a_full_reset_clears_everything() {
        let mut modes = TerminalModes::new();
        modes.feed(b"\x1b[?1049h\x1b[?1006h");
        modes.feed(b"\x1bc");
        assert!(modes.is_empty());
    }

    #[test]
    fn bracketed_paste_reads_as_the_shell_last_left_it() {
        // zle switches it on to read a line and off again before running it,
        // so the answer has to follow both edges rather than latch the first.
        let mut modes = TerminalModes::new();
        assert!(!modes.is_on(BRACKETED_PASTE));
        modes.feed(b"\x1b[?2004h");
        assert!(modes.is_on(BRACKETED_PASTE));
        modes.feed(b"\x1b[?2004l");
        assert!(!modes.is_on(BRACKETED_PASTE));
    }
}
