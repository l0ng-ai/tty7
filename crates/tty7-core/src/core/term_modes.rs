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
//! The kitty keyboard flags are tracked alongside, and restored differently:
//! see [`TerminalModes::replay_prelude`].
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
/// which is how a paste turns into commands. `2031` is colour scheme
/// notifications: a reattached window that lost it never tells the program the
/// theme flipped.
#[rustfmt::skip]
const TRACKED: &[u16] = &[
    1, 47, 1047, 1049, 1000, 1002, 1003, 1004, 1005, 1006, 1007, 1015, 1016, 2004,
    COLOR_SCHEME_UPDATES,
];

/// DEC mode 2031: report light/dark changes as `CSI ? 997 ; 1|2 n`.
pub const COLOR_SCHEME_UPDATES: u16 = 2031;

/// Bracketed paste: whether a paste may arrive framed as `ESC[200~ … ESC[201~`.
pub const BRACKETED_PASTE: u16 = 2004;

/// The alternate screen the client's emulator swaps on. It ignores `47` and
/// `1047`, so they leave the keyboard stacks where they are.
const ALT_SCREEN: u16 = 1049;

/// A CSI longer than this is not a mode set; keep the buffer bounded.
const MAX_PARAMS: usize = 64;

/// How deep a keyboard flags stack gets before its bottom entry is dropped —
/// the client emulator's own limit (`KEYBOARD_MODE_STACK_MAX_DEPTH` in
/// `alacritty_terminal`), so a deep run of pushes is cut where it is cut there.
const KEYBOARD_STACK_MAX_DEPTH: usize = 4096;

/// The flag bits the protocol defines; the emulator truncates the rest.
const KEYBOARD_FLAGS: u8 = 0b1_1111;

/// The kitty keyboard flags, kept the way the client's emulator keeps them.
///
/// A program asks for the protocol with `CSI > flags u`, which pushes onto a
/// stack, and gives it back with `CSI < n u`, which pops. Each screen has a
/// stack of its own, swapped with the screen on `?1049`. `CSI = flags ; how u`
/// changes only the flags in effect, not the stack, and every push, pop and
/// swap reloads the flags in effect from the top of the stack.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct KeyboardStacks {
    primary: Vec<u8>,
    alternate: Vec<u8>,
    on_alt: bool,
    active: u8,
}

impl KeyboardStacks {
    fn current(&mut self) -> &mut Vec<u8> {
        if self.on_alt {
            &mut self.alternate
        } else {
            &mut self.primary
        }
    }

    fn reload(&mut self) {
        self.active = self.current().last().copied().unwrap_or(0);
    }

    fn push(&mut self, flags: u8) {
        let stack = self.current();
        if stack.len() >= KEYBOARD_STACK_MAX_DEPTH {
            stack.remove(0);
        }
        stack.push(flags);
        self.reload();
    }

    fn pop(&mut self, n: u16) {
        let stack = self.current();
        stack.truncate(stack.len().saturating_sub(n as usize));
        self.reload();
    }

    fn set(&mut self, flags: u8, how: u16) {
        self.active = match how {
            2 => self.active | flags,
            3 => self.active & !flags,
            _ => flags,
        };
    }

    fn swap(&mut self) {
        self.on_alt = !self.on_alt;
        self.reload();
    }

    /// The bytes that rebuild these stacks on a fresh emulator, around `dec`
    /// (mode sets that may include the switch to the alternate screen).
    ///
    /// The primary stack is pushed first, while the emulator is still on the
    /// primary screen. The alternate one can only be pushed once `dec` has
    /// switched to it, which is what `alt_follows` says; without it the
    /// alternate stack is left out, since there is no way into the alternate
    /// screen that does not clear it.
    fn rebuild_around(&self, dec: Vec<u8>, alt_follows: bool) -> Vec<u8> {
        let mut out = Vec::new();
        push_flags(&mut out, &self.primary);
        if !self.on_alt {
            self.set_active(&mut out, &self.primary);
        }
        out.extend(dec);
        if alt_follows {
            push_flags(&mut out, &self.alternate);
            if self.on_alt {
                self.set_active(&mut out, &self.alternate);
            }
        }
        out
    }

    /// A `CSI =` set the stack does not record: only needed when the flags in
    /// effect are not the ones a push would leave.
    fn set_active(&self, out: &mut Vec<u8>, stack: &[u8]) {
        if self.active != stack.last().copied().unwrap_or(0) {
            out.extend_from_slice(format!("\x1b[={};1u", self.active).as_bytes());
        }
    }
}

fn push_flags(out: &mut Vec<u8>, stack: &[u8]) {
    for flags in stack {
        out.extend_from_slice(format!("\x1b[>{flags}u").as_bytes());
    }
}

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
    /// `CSI ? 996 n` queries seen and not yet taken — see
    /// [`Self::take_color_scheme_queries`].
    color_scheme_queries: usize,
    keyboard: KeyboardStacks,
}

#[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    #[default]
    Text,
    Esc,
    /// A CSI whose parameter bytes are being read. `marker` is the private
    /// marker it opened with, `0` for none: `?` makes it a DEC private mode
    /// rather than an ANSI one, and `>`, `<` and `=` before a `u` are the
    /// kitty keyboard push, pop and set.
    Csi {
        marker: u8,
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

    /// How many `CSI ? 996 n` (which scheme is on?) queries were fed since the
    /// last call. The tracker only counts them; answering is the caller's job.
    pub fn take_color_scheme_queries(&mut self) -> usize {
        std::mem::take(&mut self.color_scheme_queries)
    }

    /// The bytes that put a freshly reset terminal back into these modes, or
    /// `None` when there is nothing to restore.
    pub fn restore_bytes(&self) -> Option<Vec<u8>> {
        let dec = Self::bytes_for(&self.on).unwrap_or_default();
        let bytes = self.keyboard.rebuild_around(dec, self.is_on(ALT_SCREEN));
        (!bytes.is_empty()).then_some(bytes)
    }

    /// Everything a client has to be told ahead of a replay of the ring.
    ///
    /// `self` is the fold of the pane's whole output, `head` the fold of what
    /// the ring has dropped from its front, and `replayed` the fold of what it
    /// still holds.
    ///
    /// The DEC modes are the ones in [`Self::restore_bytes_beyond`]. The kitty
    /// keyboard flags cannot be restored that way, as the state they ended in:
    /// they are a stack, and the pushes and pops still in the ring act on it
    /// relative to where it stood when they were written. A program that
    /// pushed once at startup and has popped and pushed around every subprocess
    /// since would come back one entry short, or one too deep. So the stacks go
    /// back the way they stood at the front of the ring — `head`'s — and the
    /// ring's own pushes and pops take them on from there to where the pane is.
    ///
    /// Without them a client that reattaches to a long-running program which
    /// pushed its flags out of the ring encodes every key in the legacy form
    /// while the program still parses the kitty one: Escape arrives as a bare
    /// `ESC` the program waits on for the rest of a sequence (#1074).
    pub fn replay_prelude(
        &self,
        head: &TerminalModes,
        replayed: &TerminalModes,
    ) -> Option<Vec<u8>> {
        let missing = self.missing_from(replayed);
        let dec = Self::bytes_for(&missing).unwrap_or_default();
        let bytes = head
            .keyboard
            .rebuild_around(dec, missing.contains(&ALT_SCREEN));
        (!bytes.is_empty()).then_some(bytes)
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
        Self::bytes_for(&self.missing_from(replayed))
    }

    fn missing_from(&self, replayed: &TerminalModes) -> Vec<u16> {
        self.on
            .iter()
            .copied()
            .filter(|mode| !replayed.on.contains(mode))
            .collect()
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
                        self.state = State::Csi { marker: 0 };
                    }
                    b']' => {
                        self.params.clear();
                        self.state = State::Osc;
                    }
                    // RIS. Everything this tracker knows goes back to default,
                    // exactly as it does in the client's emulator.
                    b'c' => {
                        self.on.clear();
                        self.keyboard = KeyboardStacks::default();
                        self.state = State::Text;
                    }
                    0x1b => {}
                    _ => self.state = State::Text,
                },
                State::Csi { marker } => match b {
                    b'?' | b'>' | b'<' | b'=' if marker == 0 && self.params.is_empty() => {
                        self.state = State::Csi { marker: b }
                    }
                    b'0'..=b'9' | b';' | b':' => {
                        self.params.push(b);
                        if self.params.len() > MAX_PARAMS {
                            self.state = State::Text;
                        }
                    }
                    b'h' | b'l' => {
                        if marker == b'?' {
                            self.apply(b == b'h');
                        }
                        self.state = State::Text;
                    }
                    b'u' => {
                        self.keyboard_op(marker);
                        self.state = State::Text;
                    }
                    b'n' => {
                        if marker == b'?' && self.params == b"996" {
                            self.color_scheme_queries += 1;
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
                    0x07 => self.osc_end(),
                    0x1b => self.state = State::OscEsc,
                    _ if self.params.len() < 5 => self.params.push(b),
                    _ => {}
                },
                State::OscEsc => match b {
                    b'\\' => self.osc_end(),
                    0x1b => {}
                    _ => self.state = State::Osc,
                },
            }
            i += 1;
        }
    }

    /// A shell prompt mark (OSC 133 `A`, `B` or `D`) means the shell owns the
    /// terminal again, so whatever program switched 2031 on is gone — possibly
    /// killed before it could send `?2031l`. A report after that would be
    /// typed into the shell's command line.
    fn osc_end(&mut self) {
        if matches!(self.params.as_slice(), b"133;A" | b"133;B" | b"133;D") {
            self.on.retain(|m| *m != COLOR_SCHEME_UPDATES);
        }
        self.params.clear();
        self.state = State::Text;
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
            // The emulator swaps screens only when the mode actually changes,
            // and the keyboard stacks go with the screen.
            if mode == ALT_SCREEN && self.is_on(mode) != on {
                self.keyboard.swap();
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

    /// A `CSI … u` with the given private marker. Parameters are read as the
    /// emulator reads them: the first sub-parameter of each, saturating, with
    /// `0` standing for the default.
    fn keyboard_op(&mut self, marker: u8) {
        let mut params = self.params.split(|b| *b == b';').map(|param| {
            param
                .iter()
                .take_while(|b| b.is_ascii_digit())
                .fold(0u16, |n, b| {
                    n.saturating_mul(10).saturating_add(u16::from(b - b'0'))
                })
        });
        let mut next_or = |default: u16| params.next().filter(|n| *n != 0).unwrap_or(default);
        let flags = |n: u16| (n as u8) & KEYBOARD_FLAGS;
        match marker {
            b'>' => self.keyboard.push(flags(next_or(0))),
            b'<' => self.keyboard.pop(next_or(1)),
            b'=' => {
                let mode = flags(next_or(0));
                self.keyboard.set(mode, next_or(1));
            }
            _ => {}
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

    fn keyboard(bytes: &[u8]) -> KeyboardStacks {
        let mut modes = TerminalModes::new();
        modes.feed(bytes);
        modes.keyboard
    }

    #[test]
    fn kitty_keyboard_pushes_and_pops_a_stack() {
        let k = keyboard(b"\x1b[>1u\x1b[>7u");
        assert_eq!((k.primary.as_slice(), k.active), (&[1, 7][..], 7));
        // A bare pop is one; a pop of zero is the default too.
        let k = keyboard(b"\x1b[>1u\x1b[>7u\x1b[<u");
        assert_eq!((k.primary.as_slice(), k.active), (&[1][..], 1));
        let k = keyboard(b"\x1b[>1u\x1b[>7u\x1b[<0u");
        assert_eq!(k.primary, [1]);
        let k = keyboard(b"\x1b[>1u\x1b[<5u");
        assert_eq!((k.primary.as_slice(), k.active), (&[][..], 0));
    }

    #[test]
    fn a_kitty_keyboard_set_changes_the_flags_in_effect_not_the_stack() {
        let k = keyboard(b"\x1b[>1u\x1b[=8;2u");
        assert_eq!((k.primary.as_slice(), k.active), (&[1][..], 9));
        let k = keyboard(b"\x1b[>3u\x1b[=1;3u");
        assert_eq!(k.active, 2);
        let k = keyboard(b"\x1b[>3u\x1b[=4u");
        assert_eq!(k.active, 4);
        // A pop reloads from the stack, dropping the set.
        let k = keyboard(b"\x1b[>1u\x1b[>1u\x1b[=0;1u\x1b[<u");
        assert_eq!(k.active, 1);
    }

    #[test]
    fn each_screen_keeps_its_own_kitty_keyboard_stack() {
        let k = keyboard(b"\x1b[>1u\x1b[?1049h\x1b[>7u");
        assert_eq!(
            (k.primary.as_slice(), k.alternate.as_slice(), k.active),
            (&[1][..], &[7][..], 7)
        );
        let k = keyboard(b"\x1b[>1u\x1b[?1049h\x1b[>7u\x1b[?1049l");
        assert_eq!(k.active, 1);
        // Re-entering a screen already on is no swap; `47` is none at all.
        let k = keyboard(b"\x1b[?1049h\x1b[>7u\x1b[?1049h\x1b[?47l");
        assert!(k.on_alt && k.active == 7);
    }

    #[test]
    fn other_csi_u_and_a_reset_leave_the_kitty_keyboard_stack_empty() {
        // Restore cursor, a flags query, and flag bits the protocol lacks.
        let k = keyboard(b"\x1b[u\x1b[?u\x1b[>224u");
        assert_eq!(k.primary, [0]);
        assert_eq!(keyboard(b"\x1b[>1u\x1bc"), KeyboardStacks::default());
    }

    #[test]
    fn a_kitty_keyboard_stack_is_capped_like_the_emulators() {
        let mut bytes = b"\x1b[>2u".to_vec();
        bytes.extend(b"\x1b[>1u".repeat(KEYBOARD_STACK_MAX_DEPTH));
        let k = keyboard(&bytes);
        assert_eq!(k.primary.len(), KEYBOARD_STACK_MAX_DEPTH);
        assert!(k.primary.iter().all(|f| *f == 1), "the bottom one went");
    }

    /// The primary stack is pushed before the alternate screen is entered, and
    /// the alternate one after; a set rides on the screen it was made on.
    #[test]
    fn restore_bytes_rebuild_each_kitty_keyboard_stack_on_its_own_screen() {
        let mut modes = TerminalModes::new();
        modes.feed(b"\x1b[>1u\x1b[?1049h\x1b[>7u\x1b[=3u");
        assert_eq!(
            modes.restore_bytes().unwrap(),
            b"\x1b[>1u\x1b[?1049h\x1b[>7u\x1b[=3;1u".to_vec()
        );
        let mut modes = TerminalModes::new();
        modes.feed(b"\x1b[>1u");
        assert_eq!(modes.restore_bytes().unwrap(), b"\x1b[>1u".to_vec());
    }

    #[test]
    fn the_replay_prelude_restores_kitty_keyboard_flags_as_the_ring_found_them() {
        let (dropped, kept) = (&b"\x1b[>1u"[..], &b"\x1b[<u\x1b[>1u"[..]);
        let (mut whole, mut head, mut ring) = (
            TerminalModes::new(),
            TerminalModes::new(),
            TerminalModes::new(),
        );
        whole.feed(dropped);
        whole.feed(kept);
        head.feed(dropped);
        ring.feed(kept);
        assert_eq!(
            whole.replay_prelude(&head, &ring).unwrap(),
            b"\x1b[>1u".to_vec()
        );
        assert!(
            whole
                .replay_prelude(&TerminalModes::new(), &whole)
                .is_none(),
            "a ring that dropped nothing needs no prelude"
        );
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

    #[test]
    fn follows_colour_scheme_updates_and_counts_its_queries() {
        let mut modes = TerminalModes::new();
        modes.feed(b"\x1b[?2031h\x1b[?99");
        assert!(modes.is_on(COLOR_SCHEME_UPDATES));
        modes.feed(b"6n\x1b[?996n\x1b[996n\x1b[?997;1n");
        assert_eq!(
            modes.take_color_scheme_queries(),
            2,
            "a split query counts, a non-private one does not"
        );
        assert_eq!(modes.take_color_scheme_queries(), 0);
        modes.feed(b"\x1b[?2031l");
        assert!(!modes.is_on(COLOR_SCHEME_UPDATES));
    }

    #[test]
    fn a_shell_prompt_ends_colour_scheme_updates() {
        for mark in [
            &b"\x1b]133;A\x07"[..],
            b"\x1b]133;B\x1b\\",
            b"\x1b]133;D;0\x07",
        ] {
            let mut modes = TerminalModes::new();
            modes.feed(b"\x1b[?2031h\x1b[?2004h\x1b]133;C\x07");
            assert!(
                modes.is_on(COLOR_SCHEME_UPDATES),
                "a command start keeps it"
            );
            modes.feed(mark);
            assert!(!modes.is_on(COLOR_SCHEME_UPDATES));
            assert!(
                modes.is_on(BRACKETED_PASTE),
                "only 2031 is the prompt's to clear"
            );
        }
        let mut modes = TerminalModes::new();
        modes.feed(b"\x1b[?2031h\x1b]0;133;A\x07\x1b]1337;A\x07");
        assert!(
            modes.is_on(COLOR_SCHEME_UPDATES),
            "other OSCs leave it alone"
        );
    }
}
