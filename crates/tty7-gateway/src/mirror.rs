//! A pane's screen as the desktop has it, drawn afresh for a phone.
//!
//! What the daemon hands an observer is the pane's raw output, ring segment by
//! ring segment, each at the size the pane was then. A desktop window plays
//! that back through alacritty and gets the screen it left. The phone plays it
//! through xterm.js, which wraps and reflows across those resizes in its own
//! way, so a shell's SIGWINCH redraws land on rows they were not meant for: a
//! prompt drawn twice, a program's output under the next prompt, half a logo
//! over yesterday's `ls`. The bytes are right; only the emulator that reads
//! them differs.
//!
//! So the gateway reads them with the desktop's own emulator, and the phone is
//! sent what that makes of them — the scrollback and the screen as plain
//! text and colours, the cursor, the modes that matter — at the moments its
//! own reading would part from the desktop's: on opening, after the pane
//! changes size, and when a full-screen program gives the main screen back.
//! Between those, output goes straight through, as before.

use alacritty_terminal::event::VoidListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::term::{Config, Term, TermMode};
use alacritty_terminal::vte::ansi::{Color, NamedColor, Processor};
use std::fmt::Write as _;

/// How much history a fresh drawing carries: what the phone keeps itself
/// (main.ts `scrollback`).
const HISTORY: usize = 5000;

struct Size {
    cols: usize,
    rows: usize,
}

impl Dimensions for Size {
    fn total_lines(&self) -> usize {
        self.rows
    }
    fn screen_lines(&self) -> usize {
        self.rows
    }
    fn columns(&self) -> usize {
        self.cols
    }
}

pub struct Mirror {
    term: Term<VoidListener>,
    parser: Processor,
}

impl Mirror {
    pub fn new(cols: u16, rows: u16) -> Mirror {
        let config = Config {
            scrolling_history: HISTORY,
            ..Config::default()
        };
        Mirror {
            term: Term::new(config, &size(cols, rows), VoidListener),
            parser: Processor::new(),
        }
    }

    pub fn resize(&mut self, cols: u16, rows: u16) {
        self.term.resize(size(cols, rows));
    }

    /// Reads output. True when it brought a full-screen program's exit: the
    /// main screen is back, and the phone's copy of it is the one that went
    /// stale while the program ran.
    pub fn feed(&mut self, bytes: &[u8]) -> bool {
        let was_alt = self.alt();
        self.parser.advance(&mut self.term, bytes);
        was_alt && !self.alt()
    }

    fn alt(&self) -> bool {
        self.term.mode().contains(TermMode::ALT_SCREEN)
    }

    /// The whole picture, for a terminal of the same width to read from a
    /// reset: history, screen, cursor and modes. The screen is written last
    /// and the cursor placed relative to its end, so a terminal taller than
    /// the pane shows it at the bottom with history above.
    pub fn draw(&self) -> Vec<u8> {
        let mut out = String::new();
        // Everything back to the start, scrollback too.
        out.push_str("\x1bc\x1b[3J");
        let alt = self.alt();
        if alt {
            out.push_str("\x1b[?1049h");
        }
        let grid = self.term.grid();
        let cols = grid.columns();
        let rows = grid.screen_lines() as i32;
        // The alternate screen keeps no history.
        let history = if alt { 0 } else { grid.history_size() as i32 };
        let mut pen = Pen::default();
        for line in -history..rows {
            let row = &grid[Line(line)];
            let wrapped = row[Column(cols - 1)].flags.contains(Flags::WRAPLINE);
            // Blank cells at the end of a line are left unwritten unless they
            // carry a colour; a wrapped line is written whole, so the reader
            // wraps it too and knows the two rows are one line.
            let end = if wrapped {
                cols
            } else {
                (0..cols)
                    .rposition(|c| !blank(&row[Column(c)]))
                    .map_or(0, |c| c + 1)
            };
            for c in 0..end {
                let cell = &row[Column(c)];
                if cell
                    .flags
                    .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
                {
                    continue;
                }
                pen.to(cell, &mut out);
                out.push(cell.c);
                if let Some(marks) = cell.zerowidth() {
                    out.extend(marks);
                }
            }
            if line < rows - 1 && !wrapped {
                pen.reset(&mut out);
                out.push_str("\r\n");
            }
        }
        pen.reset(&mut out);

        let cursor = grid.cursor.point;
        let up = rows - 1 - cursor.line.0;
        if up > 0 {
            let _ = write!(out, "\x1b[{up}A");
        }
        let _ = write!(out, "\x1b[{}G", cursor.column.0 + 1);

        let mode = self.term.mode();
        for (flag, set) in [
            (TermMode::APP_CURSOR, "?1"),
            (TermMode::BRACKETED_PASTE, "?2004"),
            (TermMode::FOCUS_IN_OUT, "?1004"),
            (TermMode::MOUSE_REPORT_CLICK, "?1000"),
            (TermMode::MOUSE_DRAG, "?1002"),
            (TermMode::MOUSE_MOTION, "?1003"),
            (TermMode::SGR_MOUSE, "?1006"),
        ] {
            if mode.contains(flag) {
                let _ = write!(out, "\x1b[{set}h");
            }
        }
        if mode.contains(TermMode::APP_KEYPAD) {
            out.push_str("\x1b=");
        }
        if !mode.contains(TermMode::SHOW_CURSOR) {
            out.push_str("\x1b[?25l");
        }
        out.into_bytes()
    }
}

fn size(cols: u16, rows: u16) -> Size {
    // A zero-sized grid panics inside alacritty's index arithmetic.
    Size {
        cols: usize::from(cols).max(2),
        rows: usize::from(rows).max(1),
    }
}

fn blank(cell: &Cell) -> bool {
    cell.c == ' '
        && cell.bg == Color::Named(NamedColor::Background)
        && !cell
            .flags
            .intersects(Flags::INVERSE | Flags::ALL_UNDERLINES | Flags::STRIKEOUT)
        && cell.zerowidth().is_none()
}

/// The SGR state written so far, so a run of like cells costs one sequence.
#[derive(Default, PartialEq)]
struct Pen {
    set: Option<(Flags, Color, Color)>,
}

impl Pen {
    fn to(&mut self, cell: &Cell, out: &mut String) {
        let flags = cell.flags
            & (Flags::BOLD
                | Flags::DIM
                | Flags::ITALIC
                | Flags::ALL_UNDERLINES
                | Flags::INVERSE
                | Flags::HIDDEN
                | Flags::STRIKEOUT);
        let want = (flags, cell.fg, cell.bg);
        if self.set == Some(want) {
            return;
        }
        out.push_str("\x1b[0");
        for (flag, code) in [
            (Flags::BOLD, "1"),
            (Flags::DIM, "2"),
            (Flags::ITALIC, "3"),
            (Flags::INVERSE, "7"),
            (Flags::HIDDEN, "8"),
            (Flags::STRIKEOUT, "9"),
        ] {
            if flags.contains(flag) {
                out.push(';');
                out.push_str(code);
            }
        }
        if flags.contains(Flags::DOUBLE_UNDERLINE) {
            out.push_str(";21");
        } else if flags.intersects(Flags::ALL_UNDERLINES) {
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

fn colour(color: Color, bg: bool, out: &mut String) {
    let base = if bg { 40 } else { 30 };
    match color {
        Color::Spec(rgb) => {
            let _ = write!(out, ";{};2;{};{};{}", base + 8, rgb.r, rgb.g, rgb.b);
        }
        Color::Indexed(i) => {
            let _ = write!(out, ";{};5;{i}", base + 8);
        }
        Color::Named(named) => {
            let index = match named {
                NamedColor::Foreground | NamedColor::Background => return,
                NamedColor::DimBlack => 0,
                NamedColor::DimRed => 1,
                NamedColor::DimGreen => 2,
                NamedColor::DimYellow => 3,
                NamedColor::DimBlue => 4,
                NamedColor::DimMagenta => 5,
                NamedColor::DimCyan => 6,
                NamedColor::DimWhite => 7,
                NamedColor::BrightForeground | NamedColor::DimForeground | NamedColor::Cursor => {
                    return;
                }
                other => other as usize,
            };
            let code = if index < 8 {
                base + index
            } else {
                base + 60 + index - 8
            };
            let _ = write!(out, ";{code}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What a terminal shows after reading `bytes`, as text, through the same
    /// emulator: the drawing has to read back as the screen it was made from.
    fn screen(mirror: &Mirror) -> Vec<String> {
        let grid = mirror.term.grid();
        let history = grid.history_size() as i32;
        (-history..grid.screen_lines() as i32)
            .map(|l| {
                let row = &grid[Line(l)];
                (0..grid.columns())
                    .map(|c| row[Column(c)].c)
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    fn redrawn(mirror: &Mirror) -> Mirror {
        let grid = mirror.term.grid();
        let mut copy = Mirror::new(grid.columns() as u16, grid.screen_lines() as u16);
        copy.feed(&mirror.draw());
        copy
    }

    #[test]
    fn a_drawing_reads_back_as_the_screen_it_was_made_from() {
        let mut m = Mirror::new(20, 4);
        m.feed(b"one\r\ntwo\r\n\x1b[31mred\x1b[0m three\r\nfour\r\nfive $ ");
        let copy = redrawn(&m);
        assert_eq!(screen(&copy), screen(&m));
        assert_eq!(copy.term.grid().cursor.point, m.term.grid().cursor.point);
        // Colour survives.
        let red = |m: &Mirror| {
            let grid = m.term.grid();
            let history = grid.history_size() as i32;
            (-history..grid.screen_lines() as i32)
                .flat_map(|l| (0..grid.columns()).map(move |c| (l, c)))
                .find(|&(l, c)| {
                    grid[Line(l)][Column(c)].c == 'r' && grid[Line(l)][Column(c + 1)].c == 'e'
                })
                .map(|(l, c)| grid[Line(l)][Column(c)].fg)
        };
        assert_eq!(red(&copy), Some(Color::Named(NamedColor::Red)));
    }

    #[test]
    fn a_line_longer_than_the_pane_stays_one_line() {
        let mut m = Mirror::new(10, 3);
        m.feed(b"abcdefghijklmno\r\n$ ");
        let copy = redrawn(&m);
        assert_eq!(screen(&copy), screen(&m));
        let grid = copy.term.grid();
        let first = grid.history_size() as i32;
        let wrapped = (-first..grid.screen_lines() as i32)
            .any(|l| grid[Line(l)][Column(9)].flags.contains(Flags::WRAPLINE));
        assert!(wrapped, "the long line is still marked as wrapping");
    }

    #[test]
    fn a_full_screen_program_is_drawn_on_the_alternate_screen() {
        let mut m = Mirror::new(20, 4);
        m.feed(b"$ less\r\n\x1b[?1049h\x1b[2J\x1b[1;1Hpage one\x1b[4;1H:");
        let copy = redrawn(&m);
        assert!(copy.alt());
        assert_eq!(screen(&copy), screen(&m));
        assert_eq!(copy.term.grid().cursor.point, m.term.grid().cursor.point);
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
}
