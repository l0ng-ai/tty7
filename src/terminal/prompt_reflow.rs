//! Reflows a pane's grid without stranding the prompt the shell is about to
//! redraw (#654).
//!
//! A shell sitting at its prompt answers a `SIGWINCH` by redrawing the prompt
//! relative to where it left the cursor: zsh and bash move up as many rows as
//! the prompt's last line and the command being edited took at the width they
//! last drew at, return to column 0, and clear everything below. A column
//! change reflows the grid underneath that arithmetic. A prompt whose right
//! side sits near the edge — an `RPROMPT` clock, a right-aligned segment —
//! wraps onto a second row when the pane narrows by two columns or more, the
//! grid keeps the cursor on the row holding the wrapped tail, and the redraw
//! starts there: the head of the old prompt stays behind, still marked as
//! wrapping into the new one. Every such step of a window drag leaves another,
//! and a later widening joins them into one long line of prompt fragments
//! across the pane — the shell and the grid agreed on the width the whole time.
//!
//! So a reflow at a prompt hands the prompt back first, the way kitty does for
//! a shell with integration: the cursor's line, from its first row down, is
//! the shell's to repaint and is cleared, and after the reflow the cursor is
//! put back as many rows below that line's start as it was before — the count
//! the shell is about to move up by. A second reflow that lands before the
//! shell has redrawn (a drag outrunning the prompt) finds those rows blank and
//! unwrapped, so it leaves them where they are and the count still holds.
//!
//! Only the cursor's own line is handed back. A multi-line prompt's upper
//! lines end in a newline, not a wrap, so they reflow like any other output;
//! they are also the rows the shell redraws without being told where they are.

use alacritty_terminal::event::EventListener;
use alacritty_terminal::grid::Dimensions as _;
use alacritty_terminal::index::{Column, Line, Point};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{MIN_COLUMNS, Term, TermMode};
use alacritty_terminal::vte::ansi::{ClearMode, Handler as _};

use super::size::TermSize;

/// Resizes `term` to `size`. `shell_redraws` says the pane is at a shell
/// prompt that will repaint itself for the new width — the caller's shell
/// integration state; everything else resizes exactly as `Term::resize` does.
pub(super) fn resize<T: EventListener>(term: &mut Term<T>, size: TermSize, shell_redraws: bool) {
    let narrows = size.cols.max(MIN_COLUMNS) < term.columns();
    let history = term.grid().history_size();
    reflow(term, size, shell_redraws);
    if narrows {
        give_back_rows_pushed_into_history(term, history);
    }
}

fn reflow<T: EventListener>(term: &mut Term<T>, size: TermSize, shell_redraws: bool) {
    // Rows alone reflow nothing, and the alternate screen is a full-screen
    // program's, which repaints the whole of it.
    let reflows = size.cols.max(MIN_COLUMNS) != term.columns();
    if !shell_redraws || !reflows || term.mode().contains(TermMode::ALT_SCREEN) {
        term.resize(size);
        return;
    }

    let cursor = term.grid().cursor.point;
    // Stops at the top of the screen: the shell's cursor-up stops there too,
    // so rows already in scrollback are not part of what it will move over.
    let start = wrapped_line_start(term, cursor.line);
    let rows_up = (cursor.line.0 - start.0) as usize;

    term.grid_mut().cursor.point = Point::new(start, Column(0));
    term.grid_mut().cursor.input_needs_wrap = false;
    term.clear_screen(ClearMode::Below);
    term.resize(size);

    // Linefeeds rather than a jump: a line that sat low on the screen has to
    // scroll the output above it up, as the shell's own layout would have.
    for _ in 0..rows_up {
        term.linefeed();
    }
    let last = Column(term.columns() - 1);
    term.grid_mut().cursor.point.column = cursor.column.min(last);
}

/// Pulls back the rows a narrowing pushed into scrollback while the screen
/// still had blank rows below the cursor to hold them.
///
/// The grid keeps a reflow anchored at the bottom: every line that wraps onto
/// one more row pushes the top row of the screen into history, however much
/// empty space sits under the cursor. Splitting a pane with a few lines of
/// output in it scrolled its first lines out of sight above a screen that was
/// mostly blank. Terminal.app, iTerm2 and kitty keep such a screen anchored at
/// the top, and so does this: the rows come back and the blank ones below the
/// cursor make room for them.
///
/// Taking rows off the bottom and adding them back is how the grid itself
/// does it — shrinking drops blank rows under the cursor, and growing pulls
/// from history first. Not under ConPTY (Windows), whose grid anchors rows to
/// what conhost last painted and pulls nothing back on growth.
fn give_back_rows_pushed_into_history<T: EventListener>(term: &mut Term<T>, history_before: usize) {
    if cfg!(windows) || term.mode().contains(TermMode::ALT_SCREEN) {
        return;
    }
    let pushed = term.grid().history_size().saturating_sub(history_before);
    let lines = term.screen_lines();
    let below_cursor = term.grid().cursor.point.line.0 as usize + 1..lines;
    let blank = below_cursor
        .rev()
        .take_while(|&line| term.grid()[Line(line as i32)].is_clear())
        .count();
    let back = pushed.min(blank);
    if back == 0 {
        return;
    }
    let cols = term.columns();
    term.resize(TermSize::new(cols, lines - back));
    term.resize(TermSize::new(cols, lines));
}

/// The two prompt marks the shell brackets its prompt with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptMark {
    /// OSC 133;A — a fresh prompt is about to be drawn.
    Opened,
    /// OSC 133;B — the prompt is drawn and input begins here.
    Ready,
}

impl PromptMark {
    /// Reads an OSC payload (`133;A`, `133;B`, …).
    pub fn parse(payload: &[u8]) -> Option<Self> {
        let mark = payload.strip_prefix(b"133;")?;
        let (&kind, rest) = mark.split_first()?;
        if !(rest.is_empty() || rest.first() == Some(&b';')) {
            return None;
        }
        match kind {
            b'A' => Some(Self::Opened),
            b'B' => Some(Self::Ready),
            _ => None,
        }
    }
}

/// Puts the line break back between output that did not end in a newline and
/// the prompt after it.
///
/// zsh's `PROMPT_SP` moves such a prompt onto a line of its own by writing
/// the `%` end-of-line mark and then padding it with spaces until the cursor
/// *wraps*. To the grid that is one wrapped line: `printf xyz` and the prompt
/// under it read as a single line of text, so the reflow above took the
/// output for the prompt's own first row and erased it on the next resize,
/// and widening the pane joined the two onto one row.
///
/// The marks say where that happened. `A` comes from precmd, before
/// `PROMPT_SP`: a cursor off column 0 there is output left dangling. By `B`
/// the prompt is drawn, and if the row that output ended on is now padding
/// all the way to its last column and wraps into the rows below, that wrap is
/// the padding's, not the text's — so it becomes a hard break. A prompt that
/// simply carried on from the output (bash has no `PROMPT_SP`) leaves its own
/// text on that row rather than blanks, and is left alone.
#[derive(Default)]
pub struct PromptBreak {
    /// Where `A` found a dangling line: how many rows into its wrapped line
    /// the cursor was, the column, and the character it had just written —
    /// the last to recognize the row by again at `B`, which the prompt may
    /// have scrolled.
    dangling: Option<(usize, Column, char)>,
}

impl PromptBreak {
    pub fn apply<T: EventListener>(&mut self, term: &mut Term<T>, mark: PromptMark) {
        match mark {
            PromptMark::Opened => {
                let cursor = term.grid().cursor.point;
                // A full row leaves the cursor parked on its last cell: the
                // text ends there, one column short of where it stands.
                let end = match term.grid().cursor.input_needs_wrap {
                    true => cursor.column + 1,
                    false => cursor.column,
                };
                self.dangling = (end.0 > 0).then(|| {
                    let into = (cursor.line.0 - wrapped_line_start(term, cursor.line).0) as usize;
                    let last = term.grid()[cursor.line][end - 1].c;
                    (into, end, last)
                });
            }
            PromptMark::Ready => {
                let Some((into, end, last)) = self.dangling.take() else {
                    return;
                };
                let cursor = term.grid().cursor.point.line;
                let row = wrapped_line_start(term, cursor) + into;
                if row >= cursor {
                    return;
                }
                let cols = term.columns();
                let grid = term.grid();
                let padded = end.0 < cols
                    && grid[row][end - 1].c == last
                    && (end.0 + 1..cols).all(|col| grid[row][Column(col)].c == ' ')
                    && grid[row][Column(cols - 1)].flags.contains(Flags::WRAPLINE);
                if padded {
                    term.grid_mut()[row][Column(cols - 1)]
                        .flags
                        .remove(Flags::WRAPLINE);
                }
            }
        }
    }
}

/// The first row of the wrapped line `line` is part of, stopping at the top of
/// the screen as the shell's own cursor-up does.
fn wrapped_line_start<T>(term: &Term<T>, line: Line) -> Line {
    let last = Column(term.columns() - 1);
    let mut start = line;
    while start > Line(0)
        && term.grid()[Line(start.0 - 1)][last]
            .flags
            .contains(Flags::WRAPLINE)
    {
        start -= 1;
    }
    start
}

#[cfg(test)]
mod tests {
    use super::*;
    use alacritty_terminal::event::VoidListener;
    use alacritty_terminal::term::Config;
    use alacritty_terminal::vte::ansi::Processor;

    const PRIOR: &[u8] = b"echo hi\r\nhi\r\n";
    const CLOCK: &str = "16:46:33";

    fn pane(cols: usize) -> (Term<VoidListener>, Processor) {
        let mut term = Term::new(Config::default(), &TermSize::new(cols, 12), VoidListener);
        let mut parser: Processor = Processor::new();
        parser.advance(&mut term, PRIOR);
        (term, parser)
    }

    /// What zsh 5.9 writes on `SIGWINCH` for `PROMPT='[~] '` with an
    /// `RPROMPT` clock, as captured off a real pty: up the rows it last drew
    /// above the cursor, back to column 0, clear below, and lay the prompt out
    /// for `cols` — the clock right-aligned one column short of the edge, or
    /// dropped when a long command line leaves it no room.
    fn zsh_redraw(cols: usize, rows_up: usize, buffer: &str) -> Vec<u8> {
        let mut out = "\x1b[A".repeat(rows_up);
        out.push_str("\r\r\x1b[0m\x1b[J[~] ");
        if buffer.is_empty() {
            out.push_str(&format!(
                "\x1b[K\x1b[{}C{CLOCK}\x1b[{}D",
                cols - 13,
                cols - 5
            ));
        } else {
            out.push_str(buffer);
            out.push_str("\x1b[K");
        }
        out.into_bytes()
    }

    /// Every non-blank row, scrollback included, trailing blanks trimmed.
    fn text(term: &Term<VoidListener>) -> Vec<String> {
        let top = -(term.grid().history_size() as i32);
        (top..term.screen_lines() as i32)
            .map(|line| {
                let row = &term.grid()[Line(line)];
                (0..term.columns())
                    .map(|col| row[Column(col)].c)
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .filter(|row| !row.is_empty())
            .collect()
    }

    fn prompt_row(cols: usize) -> String {
        format!("[~]{}{CLOCK}", " ".repeat(cols - 12))
    }

    /// The visible rows only, top to bottom, trailing blanks trimmed.
    fn screen(term: &Term<VoidListener>) -> Vec<String> {
        (0..term.screen_lines() as i32)
            .map(|line| {
                let row = &term.grid()[Line(line)];
                (0..term.columns())
                    .map(|col| row[Column(col)].c)
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    /// Splitting a pane with a few lines in it: the long line wraps onto a
    /// second row, and the grid used to make room by scrolling `echo hi` into
    /// history above a screen that was otherwise empty.
    #[test]
    fn narrowing_a_mostly_empty_screen_keeps_its_first_rows_on_screen() {
        for shell_redraws in [false, true] {
            let (mut term, mut parser) = pane(98);
            parser.advance(&mut term, format!("{}\r\n$ ", "y".repeat(70)).as_bytes());

            resize(&mut term, TermSize::new(40, 12), shell_redraws);

            assert_eq!(term.grid().history_size(), 0, "redraws: {shell_redraws}");
            let rows = screen(&term);
            assert_eq!(
                rows[..4],
                ["echo hi", "hi", &"y".repeat(40), &"y".repeat(30)]
            );
            assert_eq!(term.grid().cursor.point.line, Line(4));
        }
    }

    /// A full screen has nowhere to put the extra rows but history, exactly as
    /// before — and what is taken back never outnumbers the blank rows.
    #[test]
    fn narrowing_a_full_screen_still_scrolls_into_history() {
        let (mut term, mut parser) = pane(98);
        for i in 0..10 {
            parser.advance(&mut term, format!("{i}{}\r\n", "z".repeat(59)).as_bytes());
        }
        // Rows 0..12 now hold `hi`, ten long lines and the cursor's empty row.
        resize(&mut term, TermSize::new(40, 12), false);

        let rows = screen(&term);
        assert_eq!(rows.last().map(String::as_str), Some(""), "{rows:#?}");
        assert_eq!(rows[rows.len() - 2], "z".repeat(20), "{rows:#?}");
        assert_eq!(term.grid().cursor.point.line, Line(11));
    }

    /// The reported pattern: a clock prompt redrawn after each step of a drag
    /// that narrows the pane by more than one column at a time, then widens it
    /// back. Measured against the unguarded resize, so the test also proves it
    /// reproduces the fragments rather than passing on a scenario that never
    /// produced any.
    #[test]
    fn a_prompt_narrowed_under_its_right_side_leaves_no_fragments() {
        let drag = |guarded: bool| {
            let (mut term, mut parser) = pane(98);
            parser.advance(&mut term, &zsh_redraw(98, 0, ""));
            for cols in [95, 92, 89, 86, 89, 92, 95, 98] {
                if guarded {
                    resize(&mut term, TermSize::new(cols, 12), true);
                } else {
                    term.resize(TermSize::new(cols, 12));
                }
                parser.advance(&mut term, &zsh_redraw(cols, 0, ""));
            }
            text(&term)
        };

        let unguarded = drag(false);
        assert!(
            unguarded.iter().filter(|row| row.contains("[~]")).count() > 1,
            "the scenario no longer strands a prompt without the guard: {unguarded:#?}"
        );
        assert_eq!(drag(true), ["echo hi", "hi", &prompt_row(98)]);
    }

    /// A command line long enough to wrap: zsh moves up by the rows it took at
    /// the old width, which the reflow changes (two rows at 98 columns, three
    /// at 60). Landing one row off either way strands a row of the old line or
    /// clears a row of the output above it.
    #[test]
    fn a_wrapped_command_line_is_redrawn_in_place() {
        let buffer = "x".repeat(150);
        let (mut term, mut parser) = pane(98);
        parser.advance(&mut term, &zsh_redraw(98, 0, &buffer));

        resize(&mut term, TermSize::new(60, 12), true);
        parser.advance(&mut term, &zsh_redraw(60, 1, &buffer));
        assert_eq!(
            text(&term),
            [
                "echo hi".to_string(),
                "hi".to_string(),
                format!("[~] {}", "x".repeat(56)),
                "x".repeat(60),
                "x".repeat(34),
            ]
        );

        resize(&mut term, TermSize::new(98, 12), true);
        parser.advance(&mut term, &zsh_redraw(98, 2, &buffer));
        assert_eq!(
            text(&term),
            [
                "echo hi".to_string(),
                "hi".to_string(),
                format!("[~] {}", "x".repeat(94)),
                "x".repeat(56),
            ]
        );
    }

    /// A drag can outrun the shell: a second reflow lands before the redraw
    /// for the first, on a line the first one already cleared. The shell still
    /// moves up by the rows of the layout it drew last, which the cleared rows
    /// above the cursor still are.
    #[test]
    fn back_to_back_reflows_before_the_redraw_strand_nothing() {
        let (mut term, mut parser) = pane(98);
        parser.advance(&mut term, &zsh_redraw(98, 0, ""));
        resize(&mut term, TermSize::new(92, 12), true);
        resize(&mut term, TermSize::new(86, 12), true);
        parser.advance(&mut term, &zsh_redraw(86, 0, ""));
        assert_eq!(text(&term), ["echo hi", "hi", &prompt_row(86)]);

        let buffer = "x".repeat(150);
        let (mut term, mut parser) = pane(98);
        parser.advance(&mut term, &zsh_redraw(98, 0, &buffer));
        resize(&mut term, TermSize::new(60, 12), true);
        resize(&mut term, TermSize::new(80, 12), true);
        parser.advance(&mut term, &zsh_redraw(80, 1, &buffer));
        assert_eq!(
            text(&term),
            [
                "echo hi".to_string(),
                "hi".to_string(),
                format!("[~] {}", "x".repeat(76)),
                "x".repeat(74),
            ]
        );
    }

    /// Low on the screen, putting the cursor back below the cleared line means
    /// scrolling: the shell's layout had those rows, so the output above moves
    /// up rather than the redraw climbing into it.
    #[test]
    fn a_line_at_the_bottom_scrolls_the_output_above_it() {
        let buffer = "x".repeat(150);
        let mut term = Term::new(Config::default(), &TermSize::new(98, 4), VoidListener);
        let mut parser: Processor = Processor::new();
        parser.advance(&mut term, b"one\r\ntwo\r\n");
        parser.advance(&mut term, &zsh_redraw(98, 0, &buffer));
        assert_eq!(term.grid().cursor.point.line, Line(3));

        resize(&mut term, TermSize::new(60, 4), true);
        parser.advance(&mut term, &zsh_redraw(60, 1, &buffer));
        assert_eq!(
            text(&term),
            [
                "one".to_string(),
                "two".to_string(),
                format!("[~] {}", "x".repeat(56)),
                "x".repeat(60),
                "x".repeat(34),
            ]
        );
    }

    /// Away from a prompt nothing is going to repaint the line, so it reflows
    /// like any output — as does a prompt whose width did not change.
    #[test]
    fn only_a_column_change_at_a_prompt_is_handed_back() {
        let (mut term, mut parser) = pane(98);
        parser.advance(&mut term, "y".repeat(150).as_bytes());
        resize(&mut term, TermSize::new(60, 12), false);
        resize(&mut term, TermSize::new(98, 12), false);
        assert_eq!(
            text(&term),
            [
                "echo hi".to_string(),
                "hi".to_string(),
                "y".repeat(98),
                "y".repeat(52)
            ]
        );

        let (mut term, mut parser) = pane(98);
        parser.advance(&mut term, &zsh_redraw(98, 0, ""));
        resize(&mut term, TermSize::new(98, 8), true);
        assert_eq!(text(&term), ["echo hi", "hi", &prompt_row(98)]);
    }

    /// The alternate screen belongs to a full-screen program, which repaints
    /// all of it; a stale prompt flag must not blank it.
    #[test]
    fn the_alternate_screen_is_left_alone() {
        let (mut term, mut parser) = pane(60);
        parser.advance(&mut term, b"\x1b[?1049h\x1b[Hstatus bar");
        resize(&mut term, TermSize::new(98, 12), true);
        assert_eq!(text(&term), ["status bar"]);
    }

    /// What zsh 5.9 writes between precmd's `A` and the prompt when the last
    /// output did not end in a newline, as captured off a real pty: the
    /// standout `%` end-of-line mark, `cols - 1` spaces to force a wrap, then
    /// back to column 0 and clear below.
    fn prompt_sp(cols: usize) -> Vec<u8> {
        let mut out = b"\x1b[1m\x1b[7m%\x1b[27m\x1b[1m\x1b[0m".to_vec();
        out.extend(std::iter::repeat_n(b' ', cols - 1));
        out.extend_from_slice(b"\r \r\r\x1b[0m\x1b[27m\x1b[24m\x1b[J");
        out
    }

    /// `printf xyz` at a zsh prompt, through a stream the reader has cut at
    /// the prompt marks, then drawn out at `cols`.
    fn dangling_output(cols: usize, guarded: bool) -> (Term<VoidListener>, Processor) {
        let (mut term, mut parser) = pane(cols);
        let mut marks = PromptBreak::default();
        parser.advance(&mut term, b"xyz");
        if guarded {
            marks.apply(&mut term, PromptMark::Opened);
        }
        parser.advance(&mut term, &prompt_sp(cols));
        parser.advance(&mut term, &zsh_redraw(cols, 0, ""));
        if guarded {
            marks.apply(&mut term, PromptMark::Ready);
        }
        (term, parser)
    }

    /// `PROMPT_SP` wraps the dangling output's row into the prompt's. Taken
    /// for the prompt's own first row, that output was cleared by the next
    /// resize at the prompt — `printf xyz` simply vanished when a panel
    /// opened. Measured against the stream without the marks' fix, so the
    /// test also proves the scenario loses the output there.
    #[test]
    fn output_without_a_newline_survives_a_resize_at_the_prompt() {
        let narrow = |guarded: bool| {
            let (mut term, mut parser) = dangling_output(98, guarded);
            resize(&mut term, TermSize::new(60, 12), true);
            parser.advance(&mut term, &zsh_redraw(60, 0, ""));
            text(&term)
        };
        assert!(
            !narrow(false).iter().any(|row| row.starts_with("xyz")),
            "the scenario no longer loses the output without the fix"
        );
        assert_eq!(narrow(true), ["echo hi", "hi", "xyz%", &prompt_row(60)]);
    }

    /// Widening joins wrapped rows back together, and the padding's wrap
    /// joined the output and the prompt onto one row.
    #[test]
    fn widening_keeps_dangling_output_off_the_prompt_row() {
        let (mut term, mut parser) = dangling_output(60, true);
        resize(&mut term, TermSize::new(98, 12), true);
        parser.advance(&mut term, &zsh_redraw(98, 0, ""));
        assert_eq!(text(&term), ["echo hi", "hi", "xyz%", &prompt_row(98)]);
    }

    /// A prompt that carries straight on from the output — bash has no
    /// `PROMPT_SP` — wraps on its own text, not on padding, and that wrap is
    /// the shell's to count.
    #[test]
    fn a_prompt_continuing_the_output_row_keeps_its_wrap() {
        let (mut term, mut parser) = pane(20);
        let mut marks = PromptBreak::default();
        parser.advance(&mut term, b"xyz");
        marks.apply(&mut term, PromptMark::Opened);
        parser.advance(&mut term, b"user@host:~/some/long/path$ ");
        marks.apply(&mut term, PromptMark::Ready);
        let row: Line = term.grid().cursor.point.line - 1;
        assert!(term.grid()[row][Column(19)].flags.contains(Flags::WRAPLINE));
    }

    #[test]
    fn prompt_marks_parse() {
        assert_eq!(PromptMark::parse(b"133;A"), Some(PromptMark::Opened));
        assert_eq!(PromptMark::parse(b"133;B"), Some(PromptMark::Ready));
        assert_eq!(PromptMark::parse(b"133;A;k=s"), Some(PromptMark::Opened));
        assert_eq!(PromptMark::parse(b"133;C"), None);
        assert_eq!(PromptMark::parse(b"133;AB"), None);
    }
}
