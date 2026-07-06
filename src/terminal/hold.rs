//! Client-side hold for keystrokes typed into the prompt→prompt gap.
//!
//! While a command runs (`at_prompt` false), typed bytes traditionally go
//! straight to the PTY, where the kernel echoes them immediately — leaving
//! `ls%`-style debris in the scrollback when the user types ahead of a fast
//! command (`cd`, `ls`…). But those bytes can't just be swallowed either: a
//! running command may be reading its stdin (a REPL, a password prompt).
//!
//! The compromise is a short hold: reconstructable gap input (printable text,
//! Backspace) is captured client-side for up to the caller's dump window
//! (~150 ms). If the editor engages first — the fast-command case — the held
//! text is handed to it verbatim and the PTY never sees a byte: no echo, no
//! wipe, pristine scrollback. If the window lapses — a long command, or a
//! program actually reading stdin — the bytes are released to the PTY exactly
//! as typed, and the rest of the gap is raw passthrough so interactive
//! programs feel no further delay. Unreconstructable input (arrows, chords,
//! Enter, multi-line pastes) releases the hold immediately and passes
//! through, preserving byte order.
//!
//! The struct is pure state — no timers, no PTY. The caller arms a timer when
//! a hold window opens (`Verdict::Held(Some(epoch))`) and calls [`GapHold::timeout`]
//! when it fires; the epoch makes a late timer firing after engage/release a
//! no-op. Two views of the held input are kept: `net`, the backspace-folded
//! text the editor (or the typeahead record) adopts, and `bytes`, the raw
//! stream a dump writes — zle folds backspaces the same way, so both views
//! converge on the same line.

/// What the hold decided to do with one gap-input event.
pub enum Verdict {
    /// Captured client-side; nothing reaches the PTY for now. `Some(epoch)` on
    /// the event that opened the window — the caller starts the dump timer
    /// with it.
    Held(Option<u64>),
    /// The gap already went raw (a dump or release happened); the caller
    /// writes the event to the PTY itself, as before holds existed.
    Passthrough,
}

#[derive(Default)]
enum State {
    /// No gap input seen since the last engage.
    #[default]
    Idle,
    /// Input is being held, dump timer running.
    Holding,
    /// The hold was dumped/released this gap; further input goes raw.
    Passthrough,
}

/// Held gap input. One per pane view; reset by [`GapHold::engage`] whenever
/// the line editor takes over.
#[derive(Default)]
pub struct GapHold {
    state: State,
    /// Backspace-folded text, as the editor would end up showing it.
    net: String,
    /// The raw byte stream exactly as typed — what a dump writes to the PTY.
    bytes: Vec<u8>,
    /// Bumped when a window opens; a dump timer carries its window's epoch so
    /// firing after engage (or after an earlier dump) is a no-op.
    epoch: u64,
}

impl GapHold {
    pub fn new() -> Self {
        Self::default()
    }

    /// Offer printable text (IME commit, single-line paste) to the hold.
    pub fn hold_text(&mut self, s: &str, bytes: &[u8]) -> Verdict {
        self.hold(bytes, |net| net.push_str(s))
    }

    /// Offer a plain Backspace to the hold. Folds the last held char off
    /// `net`; on an empty hold there is nothing shell-side to erase either
    /// (nothing was dumped), so the fold simply stays empty.
    pub fn hold_backspace(&mut self, bytes: &[u8]) -> Verdict {
        self.hold(bytes, |net| {
            net.pop();
        })
    }

    fn hold(&mut self, bytes: &[u8], fold: impl FnOnce(&mut String)) -> Verdict {
        match self.state {
            State::Passthrough => Verdict::Passthrough,
            ref s => {
                let arm = matches!(s, State::Idle).then(|| {
                    self.state = State::Holding;
                    self.epoch += 1;
                    self.epoch
                });
                fold(&mut self.net);
                self.bytes.extend_from_slice(bytes);
                Verdict::Held(arm)
            }
        }
    }

    /// An unreconstructable event (arrow, chord, Enter, multi-line paste) is
    /// about to be written raw: release whatever is held so it precedes that
    /// event on the wire, and switch the rest of the gap to passthrough.
    /// Returns `(folded_text, raw_bytes)` for the caller to write and record.
    pub fn release(&mut self) -> Option<(String, Vec<u8>)> {
        let held = matches!(self.state, State::Holding);
        self.state = State::Passthrough;
        held.then(|| {
            (
                std::mem::take(&mut self.net),
                std::mem::take(&mut self.bytes),
            )
        })
    }

    /// The dump timer for `epoch` fired: if that window is still open, release
    /// it (the command is taking long / reading stdin — the bytes must flow).
    pub fn timeout(&mut self, epoch: u64) -> Option<(String, Vec<u8>)> {
        if matches!(self.state, State::Holding) && epoch == self.epoch {
            self.release()
        } else {
            None
        }
    }

    /// The line editor engaged: whatever is still held goes to it (the PTY
    /// never saw those bytes, so there is nothing to wipe), and the next gap
    /// starts from a clean slate.
    pub fn engage(&mut self) -> Option<String> {
        self.state = State::Idle;
        self.bytes.clear();
        let net = std::mem::take(&mut self.net);
        (!net.is_empty()).then_some(net)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fast_command_gap_replays_into_the_editor_and_never_touches_the_pty() {
        let mut h = GapHold::new();
        // The first held key opens the window (the caller arms the timer)...
        assert!(matches!(h.hold_text("l", b"l"), Verdict::Held(Some(_))));
        // ...later keys ride the same window.
        assert!(matches!(h.hold_text("s", b"s"), Verdict::Held(None)));
        // The command finished inside the window: everything goes to the
        // editor; the PTY never saw a byte, so nothing echoes, nothing needs
        // a wipe.
        assert_eq!(h.engage(), Some("ls".to_string()));
        // The gap is over; the next one starts from a clean slate.
        assert_eq!(h.engage(), None);
    }

    #[test]
    fn timeout_dumps_typed_bytes_once_and_goes_passthrough() {
        let mut h = GapHold::new();
        let Verdict::Held(Some(epoch)) = h.hold_text("l", b"l") else {
            panic!("first key should open a window");
        };
        assert!(matches!(h.hold_text("s", b"s"), Verdict::Held(None)));
        // The window lapsed (long command / stdin reader): the raw bytes are
        // released for the PTY, with the folded text for the typeahead record.
        assert_eq!(h.timeout(epoch), Some(("ls".to_string(), b"ls".to_vec())));
        // The same timer can't fire twice…
        assert_eq!(h.timeout(epoch), None);
        // …and the rest of the gap is raw passthrough — no added latency for
        // whatever is reading stdin now.
        assert!(matches!(h.hold_text("x", b"x"), Verdict::Passthrough));
        // Nothing left for the editor; the next gap opens a fresh window with
        // a fresh epoch.
        assert_eq!(h.engage(), None);
        let Verdict::Held(Some(e2)) = h.hold_text("a", b"a") else {
            panic!("fresh gap should hold again");
        };
        assert_ne!(e2, epoch, "each window carries its own timer epoch");
    }

    #[test]
    fn engage_inside_the_window_cancels_the_pending_dump() {
        let mut h = GapHold::new();
        let Verdict::Held(Some(epoch)) = h.hold_text("l", b"l") else {
            panic!("first key should open a window");
        };
        assert_eq!(h.engage(), Some("l".to_string()));
        // The timer fires late, after the editor already adopted the text —
        // dumping now would type a stray "l" at the prompt.
        assert_eq!(h.timeout(epoch), None);
    }

    #[test]
    fn unreconstructable_input_releases_the_hold_in_typed_order() {
        let mut h = GapHold::new();
        h.hold_text("ls", b"ls");
        // An arrow / chord / Enter can't be replayed into the editor: what's
        // held is released first (the caller writes it, then the event's own
        // bytes — FIFO preserved), and the gap goes raw.
        assert_eq!(h.release(), Some(("ls".to_string(), b"ls".to_vec())));
        assert!(matches!(h.hold_text("x", b"x"), Verdict::Passthrough));

        // With nothing held, release still switches to passthrough, silently.
        let mut h = GapHold::new();
        assert_eq!(h.release(), None);
        assert!(matches!(h.hold_text("x", b"x"), Verdict::Passthrough));
    }

    #[test]
    fn backspace_folds_for_the_editor_but_dumps_verbatim() {
        // Editor path: the fold applies, exactly like zle would.
        let mut h = GapHold::new();
        h.hold_text("lss", b"lss");
        assert!(matches!(h.hold_backspace(b"\x7f"), Verdict::Held(None)));
        assert_eq!(h.engage(), Some("ls".to_string()));

        // Dump path: the PTY gets the stream exactly as typed (text + 0x7f);
        // the record seed uses the folded text — zle folds the same way, so
        // both views converge on the same line.
        let mut h = GapHold::new();
        let Verdict::Held(Some(e)) = h.hold_text("lss", b"lss") else {
            panic!("first key should open a window");
        };
        h.hold_backspace(b"\x7f");
        assert_eq!(h.timeout(e), Some(("ls".to_string(), b"lss\x7f".to_vec())));

        // A backspace with nothing held folds to nothing, and there is
        // nothing shell-side to erase either (nothing was dumped): it simply
        // vanishes instead of reaching the PTY.
        let mut h = GapHold::new();
        assert!(matches!(h.hold_backspace(b"\x7f"), Verdict::Held(Some(_))));
        assert_eq!(h.engage(), None);
    }
}
