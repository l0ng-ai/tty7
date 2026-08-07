//! Puts back the cursor ConPTY parks at the end of a repaint.
//!
//! conhost's VT renderer brackets every frame it paints with `?25l` … `?25h`
//! so the cursor doesn't flicker across the repaint, and it moves the cursor
//! explicitly just before the `?25h` *only on the frames where it painted the
//! cursor*. On the other frames the show commits wherever the last erase or
//! write happened to leave it — the tail of the status line, the head of a row
//! — and the cursor blinks there until conhost's next frame moves it back,
//! 7-15 ms later. A terminal that repaints when the frame lands (tty7 does,
//! on every batch of pty output) draws that stray cursor for a frame, and a
//! TUI that repaints on a spinner — Codex while it works — produces one every
//! spinner tick, which reads as a second cursor blinking in the wrong place.
//! macOS never shows it: no ConPTY sits in between, and the TUI itself always
//! moves the cursor before it shows it.
//!
//! [`ParkedCursorScanner`] finds those hide/show pairs in the byte stream and
//! [`ParkedCursorRepair`] restores the cell the cursor stood on when it went
//! invisible, which is the cell the correcting frame would have moved it back
//! to anyway.

use std::time::{Duration, Instant};

use alacritty_terminal::event::EventListener;
use alacritty_terminal::grid::Dimensions as _;
use alacritty_terminal::index::{Column, Line, Point};
use alacritty_terminal::term::Term;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorCut {
    /// `ESC [ ? 25 l` — the cursor went invisible at this offset.
    Hidden,
    /// `ESC [ ? 25 h` — the cursor came back. `parked` marks the show as one
    /// whose position is a leftover: the run since the hide moved the cursor
    /// around to paint, but ended on an erase or a write rather than on a move,
    /// so nothing chose the cell the cursor is about to appear on.
    Shown { parked: bool },
}

/// Byte scanner over the pty stream, reporting cursor hide/shows in the same
/// shape [`crate::terminal::marks::MarkScanner`] reports prompt marks: an
/// offset one past the sequence, so the reader can advance the emulator to
/// exactly there and act on the state the sequence left behind.
#[derive(Default)]
pub struct ParkedCursorScanner {
    state: State,
    /// Parameter and intermediate bytes of the CSI being read.
    csi: Vec<u8>,
    hidden: bool,
    /// The run since the hide moved the cursor at least once — it is painting a
    /// frame, not writing a line.
    saw_move: bool,
    /// The most recent thing that touched the cursor was an explicit move.
    last_was_move: bool,
}

#[derive(Default, Clone, Copy, PartialEq, Eq)]
enum State {
    #[default]
    Text,
    Esc,
    Csi,
    Osc,
    OscEsc,
}

/// CSI finals that place the cursor somewhere chosen: the cursor motions, plus
/// the save/restore pair and DECSTBM, which homes it.
const MOVES: &[u8] = b"ABCDEFGHIZ`adefsur";
/// CSI finals that leave the cursor alone: SGR, the reports and queries, window
/// operations, DECSCUSR. Anything else is a paint — erase, insert, delete,
/// scroll, repeat — and ends the run on something other than a move.
const NEUTRAL: &[u8] = b"mncpqtx";

/// A CSI longer than this is not one of ours; keep the buffer bounded.
const MAX_CSI: usize = 32;

impl ParkedCursorScanner {
    pub fn new() -> Self {
        Self::default()
    }

    /// Forgets the run in progress. A replayed snapshot rebuilds the screen
    /// from scratch, so whatever the live stream was in the middle of no longer
    /// describes the cursor on it.
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    pub fn feed(&mut self, bytes: &[u8], mut on_cut: impl FnMut(usize, CursorCut)) {
        let mut i = 0;
        while i < bytes.len() {
            if self.state == State::Text {
                let Some(off) = memchr::memchr(0x1b, &bytes[i..]) else {
                    // Plain text to the end of the batch: it wrote cells, so
                    // the run no longer ends on a move.
                    self.last_was_move = false;
                    return;
                };
                if off > 0 {
                    self.last_was_move = false;
                }
                self.state = State::Esc;
                i += off + 1;
                continue;
            }
            let b = bytes[i];
            match self.state {
                State::Text => unreachable!(),
                State::Esc => match b {
                    b'[' => {
                        self.state = State::Csi;
                        self.csi.clear();
                    }
                    b']' => self.state = State::Osc,
                    0x1b => {}
                    // Two-byte escapes: the cursor ones (DECSC/DECRC, RI, IND,
                    // NEL) place it, and reading the rest — charset
                    // designations and the like — as a move only costs a
                    // repair we skip.
                    _ => {
                        self.last_was_move = true;
                        self.state = State::Text;
                    }
                },
                State::Csi => {
                    if (0x40..=0x7e).contains(&b) {
                        self.finish_csi(b, i, &mut on_cut);
                        self.state = State::Text;
                    } else if self.csi.len() < MAX_CSI {
                        self.csi.push(b);
                    }
                    // Past MAX_CSI the sequence is longer than any of the ones
                    // this scanner reads, and a truncated buffer can't be
                    // mistaken for one: `?25` only matches when it is the whole
                    // parameter run.
                }
                // OSC carries no cursor motion; skip to its terminator.
                State::Osc => match b {
                    0x07 => self.state = State::Text,
                    0x1b => self.state = State::OscEsc,
                    _ => {}
                },
                State::OscEsc => {
                    self.state = if b == b'\\' { State::Text } else { State::Osc };
                }
            }
            i += 1;
        }
    }

    fn finish_csi(&mut self, final_byte: u8, at: usize, on_cut: &mut impl FnMut(usize, CursorCut)) {
        if self.csi.first() == Some(&b'?') {
            if self.csi == b"?25" {
                match final_byte {
                    b'l' => {
                        self.hidden = true;
                        self.saw_move = false;
                        self.last_was_move = false;
                        on_cut(at + 1, CursorCut::Hidden);
                    }
                    b'h' => {
                        let parked = self.hidden && self.saw_move && !self.last_was_move;
                        self.hidden = false;
                        on_cut(at + 1, CursorCut::Shown { parked });
                    }
                    _ => {}
                }
            }
            // Every other private mode leaves the cursor where it is.
            return;
        }
        if MOVES.contains(&final_byte) {
            self.saw_move = true;
            self.last_was_move = true;
        } else if !NEUTRAL.contains(&final_byte) {
            self.last_was_move = false;
        }
    }
}

/// The other half: keeps the cell the cursor was hidden on, and restores it
/// when the matching show turns out to be parked.
#[derive(Default)]
pub struct ParkedCursorRepair {
    hidden: Option<Hidden>,
}

struct Hidden {
    point: Point,
    input_needs_wrap: bool,
    at: Instant,
}

impl ParkedCursorRepair {
    /// Drops the remembered cell, for the same reason [`ParkedCursorScanner::reset`]
    /// drops its run.
    pub fn reset(&mut self) {
        self.hidden = None;
    }

    /// A hide and a show further apart than this are an application keeping the
    /// cursor off for the length of some work, not a renderer bracketing one
    /// frame — whatever it shows the cursor on is its own choice and stands.
    const FRAME: Duration = Duration::from_millis(100);

    pub fn apply<T: EventListener>(&mut self, term: &mut Term<T>, cut: CursorCut) {
        match cut {
            CursorCut::Hidden => {
                let cursor = &term.grid().cursor;
                let hidden = Hidden {
                    point: cursor.point,
                    input_needs_wrap: cursor.input_needs_wrap,
                    at: Instant::now(),
                };
                // A second hide while already hidden changes nothing: the cell
                // wanted is the one the cursor was last *seen* on.
                self.hidden.get_or_insert(hidden);
            }
            CursorCut::Shown { parked } => {
                let Some(hidden) = self.hidden.take() else {
                    return;
                };
                if !parked || hidden.at.elapsed() > Self::FRAME {
                    return;
                }
                let grid = term.grid_mut();
                let line = hidden
                    .point
                    .line
                    .0
                    .clamp(0, grid.screen_lines().saturating_sub(1) as i32);
                let column = hidden.point.column.0.min(grid.columns().saturating_sub(1));
                grid.cursor.point = Point::new(Line(line), Column(column));
                grid.cursor.input_needs_wrap = hidden.input_needs_wrap;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alacritty_terminal::event::VoidListener;
    use alacritty_terminal::vte::ansi::Processor;

    /// Drives a stream through the emulator the way the pty reader does —
    /// advance to each cut, act on it, carry on — and reports the cell the
    /// cursor ends on.
    fn cursor_after(stream: &[u8]) -> (i32, usize) {
        let mut term = Term::new(
            alacritty_terminal::term::Config::default(),
            &crate::terminal::size::TermSize::new(80, 24),
            VoidListener,
        );
        let mut parser: Processor = Processor::new();
        let mut scanner = ParkedCursorScanner::new();
        let mut repair = ParkedCursorRepair::default();

        let mut cuts = Vec::new();
        scanner.feed(stream, |off, cut| cuts.push((off, cut)));
        let mut at = 0;
        for (off, cut) in cuts {
            parser.advance(&mut term, &stream[at..off]);
            at = off;
            repair.apply(&mut term, cut);
        }
        parser.advance(&mut term, &stream[at..]);
        let point = term.grid().cursor.point;
        (point.line.0, point.column.0)
    }

    #[test]
    fn a_parked_show_lands_back_on_the_cell_the_frame_hid() {
        assert_eq!(
            cursor_after(b"\x1b[6;4H\x1b[?25l\x1b[20;2H\x1b[K\x1b[m\x1b[22;42H\x1b[K\x1b[?25h"),
            (5, 3),
            "the erase chose that cell, not the renderer — put the cursor back"
        );
    }

    #[test]
    fn a_show_the_frame_moved_to_is_left_where_it_is() {
        assert_eq!(
            cursor_after(
                b"\x1b[6;4H\x1b[?25l\x1b[20;2H\x1b[K\x1b[m\x1b[22;42H\x1b[K\x1b[9;9H\x1b[?25h"
            ),
            (8, 8),
            "the frame painted the cursor there on purpose"
        );
    }

    #[test]
    fn a_line_written_with_the_cursor_off_keeps_its_end_of_text_cell() {
        assert_eq!(
            cursor_after(b"\x1b[6;4H\x1b[?25l\rdone\x1b[?25h"),
            (5, 4),
            "no repaint happened, so the cursor belongs after what was written"
        );
    }

    fn cuts(stream: &[&[u8]]) -> Vec<CursorCut> {
        let mut scanner = ParkedCursorScanner::new();
        let mut got = Vec::new();
        for chunk in stream {
            scanner.feed(chunk, |_, cut| got.push(cut));
        }
        got
    }

    fn shown(stream: &[&[u8]]) -> Option<bool> {
        cuts(stream).into_iter().find_map(|cut| match cut {
            CursorCut::Shown { parked } => Some(parked),
            _ => None,
        })
    }

    #[test]
    fn offsets_land_one_past_the_sequence() {
        let mut scanner = ParkedCursorScanner::new();
        let mut got = Vec::new();
        scanner.feed(b"ab\x1b[?25lcd\x1b[?25h", |off, cut| got.push((off, cut)));
        assert_eq!(
            got,
            vec![
                (8, CursorCut::Hidden),
                (16, CursorCut::Shown { parked: false })
            ],
            "a cut must point just past its sequence, the way mark cuts do"
        );
    }

    #[test]
    fn a_frame_that_ends_on_an_erase_parks_the_cursor() {
        // What conhost sends when it repaints without painting the cursor.
        assert_eq!(
            shown(&[b"\x1b[?25l\x1b[13;37H\x1b[K\x1b[14;2H\x1b[K\x1b[m\x1b[15;42H\x1b[K\x1b[?25h"]),
            Some(true),
            "nothing chose the cell under a show that follows an erase"
        );
    }

    #[test]
    fn a_frame_that_ends_on_a_move_is_left_alone() {
        assert_eq!(
            shown(&[b"\x1b[?25l\x1b[14;2H\x1b[K\x1b[m\x1b[15;42H\x1b[K\x1b[13;3H\x1b[?25h"]),
            Some(false),
            "the renderer moved the cursor where it wants it; that position stands"
        );
    }

    #[test]
    fn colour_and_title_between_the_move_and_the_show_dont_break_the_pairing() {
        assert_eq!(
            shown(&[b"\x1b[?25l\x1b[2;5H\x1b[K\x1b[13;3H\x1b[m\x1b]0;title\x07\x1b[?25h"]),
            Some(false),
            "SGR and OSC touch no cursor, so the move before them is still the last one"
        );
    }

    #[test]
    fn hiding_the_cursor_to_write_a_line_is_not_a_frame() {
        assert_eq!(
            shown(&[b"\x1b[?25l\rworking...\x1b[?25h"]),
            Some(false),
            "with no move in the run there is no repaint to repair, and the \
             cursor belongs after the text"
        );
    }

    #[test]
    fn a_show_without_a_hide_is_never_parked() {
        assert_eq!(shown(&[b"\x1b[2;5H\x1b[K\x1b[?25h"]), Some(false));
    }

    #[test]
    fn a_frame_split_across_reads_is_still_paired() {
        assert_eq!(
            shown(&[b"\x1b[?25l\x1b[13;37H\x1b[K\x1b[15;4", b"2H\x1b[K\x1b[?25h"]),
            Some(true),
            "the pty splits frames wherever it likes; the scanner carries state"
        );
    }

    #[test]
    fn text_before_the_show_ends_the_run() {
        assert_eq!(
            shown(&[b"\x1b[?25l\x1b[13;37Hhello\x1b[?25h"]),
            Some(true),
            "a write moves the cursor as a side effect — nothing chose that cell"
        );
    }

    #[test]
    fn an_unrelated_private_mode_is_not_a_visibility_change() {
        assert!(
            cuts(&[b"\x1b[?2026h\x1b[?1049l"]).is_empty(),
            "only ?25 says anything about the cursor"
        );
    }
}
