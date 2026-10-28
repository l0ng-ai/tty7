//! A pane's screen as the desktop has it, drawn afresh for a phone.
//!
//! What the daemon hands an observer is the pane's raw output, ring segment by
//! ring segment, each at the size the pane was then. A desktop window plays
//! that back through the terminal engine and gets the screen it left. The
//! phone plays it through xterm.js, which wraps and reflows across those
//! resizes in its own way, so a shell's SIGWINCH redraws land on rows they
//! were not meant for: a prompt drawn twice, a program's output under the
//! next prompt, half a logo over yesterday's `ls`. The bytes are right; only
//! the emulator that reads them differs.
//!
//! So the gateway reads them with the desktop's own engine — through the VT
//! seam (`tty7-vt`), which today wraps the very alacritty engine the
//! desktop parses with and tomorrow wraps Ghostty without this file
//! noticing — and the phone is sent what that makes of them: the scrollback
//! and the screen as plain text and colours, the cursor, the modes that
//! matter — at the moments its own reading would part from the desktop's:
//! on opening, after the pane changes size, and when a full-screen program
//! gives the main screen back. Between those, output goes straight through,
//! as before.

use std::fmt::Write as _;

use tty7_vt::model::{Cell, CellWidth, Modes, StyleColor};
use tty7_vt::VtTerm;

/// How much history a fresh drawing carries: what the phone keeps itself
/// (main.ts `scrollback`).
const HISTORY: usize = 5000;

pub struct Mirror {
    term: VtTerm,
}

impl Mirror {
    pub fn new(cols: u16, rows: u16) -> Mirror {
        Mirror {
            term: VtTerm::new(cols, rows, HISTORY),
        }
    }

    pub fn resize(&mut self, cols: u16, rows: u16) {
        self.term.resize(cols, rows);
    }

    /// Reads output. True when it brought a full-screen program's exit: the
    /// main screen is back, and the phone's copy of it is the one that went
    /// stale while the program ran.
    pub fn feed(&mut self, bytes: &[u8]) -> bool {
        self.term.feed(bytes)
    }

    /// The whole picture, for a terminal of the same width to read from a
    /// reset: history, screen, cursor and modes. The screen is written last
    /// and the cursor placed relative to its end, so a terminal taller than
    /// the pane shows it at the bottom with history above.
    pub fn draw(&self) -> Vec<u8> {
        let snap = self.term.snapshot_full();
        let mut out = String::new();
        // Everything back to the start, scrollback too.
        out.push_str("\x1bc\x1b[3J");
        let alt = snap.modes.contains(Modes::ALT_SCREEN);
        if alt {
            out.push_str("\x1b[?1049h");
        }
        let rows = snap.screen_rows as i32;
        // The alternate screen keeps no history.
        let history = if alt { 0 } else { snap.history_rows as i32 };
        let mut pen = Pen::default();
        for line in -history..rows {
            let row = &snap.rows[(line + history) as usize];
            let wrapped = row.wrap;
            // Blank cells at the end of a line are left unwritten unless they
            // carry a colour; a wrapped line is written whole, so the reader
            // wraps it too and knows the two rows are one line.
            let end = if wrapped {
                snap.cols as usize
            } else {
                row.cells
                    .iter()
                    .rposition(|c| !c.is_blank())
                    .map_or(0, |c| c + 1)
            };
            for cell in &row.cells[..end] {
                if matches!(cell.wide, CellWidth::SpacerTail | CellWidth::SpacerHead) {
                    continue;
                }
                pen.to(cell, &mut out);
                out.push(cell.c);
                out.extend(cell.marks.iter().copied());
            }
            if line < rows - 1 && !wrapped {
                pen.reset(&mut out);
                out.push_str("\r\n");
            }
        }
        pen.reset(&mut out);

        let up = rows - 1 - snap.cursor.y as i32;
        if up > 0 {
            let _ = write!(out, "\x1b[{up}A");
        }
        let _ = write!(out, "\x1b[{}G", snap.cursor.x + 1);

        for (flag, set) in [
            (Modes::APP_CURSOR, "?1"),
            (Modes::BRACKETED_PASTE, "?2004"),
            (Modes::FOCUS_IN_OUT, "?1004"),
            (Modes::MOUSE_REPORT_CLICK, "?1000"),
            (Modes::MOUSE_DRAG, "?1002"),
            (Modes::MOUSE_MOTION, "?1003"),
            (Modes::SGR_MOUSE, "?1006"),
        ] {
            if snap.modes.contains(flag) {
                let _ = write!(out, "\x1b[{set}h");
            }
        }
        if snap.modes.contains(Modes::APP_KEYPAD) {
            out.push_str("\x1b=");
        }
        if !snap.modes.contains(Modes::SHOW_CURSOR) {
            out.push_str("\x1b[?25l");
        }
        out.into_bytes()
    }
}

/// The SGR state written so far, so a run of like cells costs one sequence.
#[derive(Default, PartialEq)]
struct Pen {
    set: Option<(u8, StyleColor, StyleColor)>,
}

impl Pen {
    fn to(&mut self, cell: &Cell, out: &mut String) {
        let mut flags = 0u8;
        if cell.bold {
            flags |= 1 << 0;
        }
        if cell.dim {
            flags |= 1 << 1;
        }
        if cell.italic {
            flags |= 1 << 2;
        }
        if cell.inverse {
            flags |= 1 << 3;
        }
        if cell.invisible {
            flags |= 1 << 4;
        }
        if cell.strikethrough {
            flags |= 1 << 5;
        }
        if cell.underline != tty7_vt::model::Underline::None {
            flags |= 1 << 6;
        }
        if cell.underline == tty7_vt::model::Underline::Double {
            flags |= 1 << 7;
        }
        let want = (flags, cell.fg, cell.bg);
        if self.set == Some(want) {
            return;
        }
        out.push_str("\x1b[0");
        for (bit, code) in [(1 << 0, "1"), (1 << 1, "2"), (1 << 2, "3"), (1 << 3, "7"), (1 << 4, "8"), (1 << 5, "9")] {
            if flags & bit != 0 {
                out.push(';');
                out.push_str(code);
            }
        }
        if flags & (1 << 7) != 0 {
            out.push_str(";21");
        } else if flags & (1 << 6) != 0 {
            out.push_str(";4");
        }
        colour(cell.fg, false, out);
        colour(cell.bg, true, out);
        out.push('m');
        self.set = Some(want);
    }

    fn reset(&mut self, out: &mut String) {
        if self.set.is_some() {
            out.push_str("\x1b[0m");
            self.set = None;
        }
    }
}

fn colour(color: StyleColor, bg: bool, out: &mut String) {
    let base = if bg { 40 } else { 30 };
    match color {
        // The theme's own default: nothing to tell the reader.
        StyleColor::None => {}
        StyleColor::Palette(index) => {
            let index = index as usize;
            let code = if index < 8 {
                base + index
            } else {
                base + 60 + index - 8
            };
            let _ = write!(out, ";{code}");
        }
        StyleColor::Rgb(rgb) => {
            let _ = write!(out, ";{};2;{};{};{}", base + 8, rgb.r, rgb.g, rgb.b);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What a terminal shows after reading `bytes`, as text, through the same
    /// emulator: the drawing has to read back as the screen it was made from.
    fn screen(mirror: &Mirror) -> Vec<String> {
        let snap = mirror.term.snapshot_full();
        snap.rows
            .iter()
            .map(|row| {
                row.cells
                    .iter()
                    .map(|c| c.c)
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    fn redrawn(mirror: &Mirror) -> Mirror {
        let snap = mirror.term.snapshot_full();
        let mut copy = Mirror::new(snap.cols, snap.screen_rows);
        copy.feed(&mirror.draw());
        copy
    }

    #[test]
    fn a_drawing_reads_back_as_the_screen_it_was_made_from() {
        let mut m = Mirror::new(20, 4);
        m.feed(b"one\r\ntwo\r\n\x1b[31mred\x1b[0m three\r\nfour\r\nfive $ ");
        let copy = redrawn(&m);
        assert_eq!(screen(&copy), screen(&m));
        let ms = m.term.snapshot_full();
        let cs = copy.term.snapshot_full();
        assert_eq!((cs.cursor.x, cs.cursor.y), (ms.cursor.x, ms.cursor.y));
        // Colour survives.
        let red = |m: &Mirror| {
            m.term
                .snapshot_full()
                .rows
                .iter()
                .flat_map(|r| r.cells.iter())
                .find(|c| c.c == 'r' && c.marks.is_empty())
                .map(|c| c.fg)
        };
        assert_eq!(red(&copy), Some(StyleColor::Palette(1)));
    }

    #[test]
    fn a_line_longer_than_the_pane_stays_one_line() {
        let mut m = Mirror::new(10, 3);
        m.feed(b"abcdefghijklmno\r\n$ ");
        let copy = redrawn(&m);
        assert_eq!(screen(&copy), screen(&m));
        let wrapped = copy
            .term
            .snapshot_full()
            .rows
            .iter()
            .any(|r| r.wrap);
        assert!(wrapped, "the long line is still marked as wrapping");
    }

    #[test]
    fn a_full_screen_program_is_drawn_on_the_alternate_screen() {
        let mut m = Mirror::new(20, 4);
        m.feed(b"$ less\r\n\x1b[?1049h\x1b[2J\x1b[1;1Hpage one\x1b[4;1H:");
        let copy = redrawn(&m);
        assert!(copy.term.alt());
        assert_eq!(screen(&copy), screen(&m));
        let ms = m.term.snapshot_full();
        let cs = copy.term.snapshot_full();
        assert_eq!((cs.cursor.x, cs.cursor.y), (ms.cursor.x, ms.cursor.y));
    }

    #[test]
    fn leaving_a_full_screen_program_is_told() {
        let mut m = Mirror::new(20, 4);
        assert!(!m.feed(b"\x1b[?1049hhi"));
        assert!(m.feed(b"\x1b[?1049l$ "));
    }

    #[test]
    fn wide_characters_take_their_two_cells_once() {
        let mut m = Mirror::new(10, 2);
        m.feed("你好 ok".as_bytes());
        let copy = redrawn(&m);
        assert_eq!(screen(&copy), screen(&m));
    }

    /// The drawing is bytes: exactly what it was before the seam, for the
    /// inputs the tests above exercise. A change here is a protocol change
    /// phones have to interop with.
    #[test]
    fn the_drawing_bytes_are_stable() {
        let mut m = Mirror::new(20, 4);
        m.feed(b"one\r\ntwo\r\n\x1b[1;31mred\x1b[0m three\r\nfour\r\nfive $ ");
        let drawing = m.draw();
        let text = String::from_utf8_lossy(&drawing);
        assert!(text.starts_with("\x1bc\x1b[3J"), "reset and purge history");
        // fg Named(Red) is palette slot 1: base 30 + 1, one run for the
        // whole "red" word, closed before " three".
        assert!(text.contains("\x1b[0;1;31mred\x1b[0m"), "bold red costs one SGR run: {text:?}");
        // The prompt "five $ " ends at column 8 of the last row.
        assert!(text.ends_with("\x1b[8G"), "the cursor sits after the $ prompt: {text:?}");
    }
}
