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

use tty59_vt::model::{GridPoint, Modes};
use tty59_vt::{MIN_COLUMNS, VtTermSession};

use super::size::TermSize;

/// Resizes the terminal behind `term` to `size`. `shell_redraws` says the
/// pane is at a shell prompt that will repaint itself for the new width —
/// the caller's shell integration state; everything else resizes exactly as
/// a plain resize does.
pub(super) fn resize(term: &mut VtTermSession, size: TermSize, shell_redraws: bool) {
    // Rows alone reflow nothing, and the alternate screen is a full-screen
    // program's, which repaints the whole of it.
    let reflows = size.cols.max(MIN_COLUMNS) != term.columns();
    if !shell_redraws || !reflows || term.modes().contains(Modes::ALT_SCREEN) {
        term.resize(size.cols, size.rows);
        return;
    }

    let cursor = term.cursor();
    let cursor_line = i32::from(cursor.y);
    // Stops at the top of the screen: the shell's cursor-up stops there too,
    // so rows already in scrollback are not part of what it will move over.
    let start = wrapped_line_start(term, cursor_line);
    let rows_up = (cursor_line - start) as usize;

    term.set_cursor(
        GridPoint {
            line: start,
            col: 0,
        },
        false,
    );
    term.clear_below();
    term.resize(size.cols, size.rows);

    // Linefeeds rather than a jump: a line that sat low on the screen has to
    // scroll the output above it up, as the shell's own layout would have.
    for _ in 0..rows_up {
        term.linefeed();
    }
    // The column the prompt had the cursor on, capped by the new width —
    // and whatever pending wrap the reflow itself left behind.
    let last = (term.columns() - 1) as u16;
    let now = term.cursor();
    term.set_cursor(
        GridPoint {
            line: i32::from(now.y),
            col: cursor.x.min(last),
        },
        now.pending_wrap,
    );
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
    dangling: Option<(usize, usize, char)>,
}

impl PromptBreak {
    pub fn apply(&mut self, term: &mut VtTermSession, mark: PromptMark) {
        match mark {
            PromptMark::Opened => {
                let cursor = term.cursor();
                let cursor_line = i32::from(cursor.y);
                // A full row leaves the cursor parked on its last cell: the
                // text ends there, one column short of where it stands.
                let end = match cursor.pending_wrap {
                    true => usize::from(cursor.x) + 1,
                    false => usize::from(cursor.x),
                };
                self.dangling = (end > 0).then(|| {
                    let into = (cursor_line - wrapped_line_start(term, cursor_line)) as usize;
                    let last = term
                        .glyph(GridPoint {
                            line: cursor_line,
                            col: (end - 1) as u16,
                        })
                        .c;
                    (into, end, last)
                });
            }
            PromptMark::Ready => {
                let Some((into, end, last)) = self.dangling.take() else {
                    return;
                };
                let cursor_line = i32::from(term.cursor().y);
                let row = wrapped_line_start(term, cursor_line) + into as i32;
                if row >= cursor_line {
                    return;
                }
                let cols = term.columns();
                let at = |col: usize| {
                    term.glyph(GridPoint {
                        line: row,
                        col: col as u16,
                    })
                    .c
                };
                let padded = end < cols
                    && at(end - 1) == last
                    && (end + 1..cols).all(|col| at(col) == ' ')
                    && term.row_wraps(row);
                if padded {
                    term.clear_row_wrap(row);
                }
            }
        }
    }
}

/// The first row of the wrapped line `line` is part of, stopping at the top of
/// the screen as the shell's own cursor-up does.
fn wrapped_line_start(term: &VtTermSession, line: i32) -> i32 {
    let mut start = line;
    while start > 0 && term.row_wraps(start - 1) {
        start -= 1;
    }
    start
}

#[cfg(test)]
mod tests {
    use super::*;

    const PRIOR: &[u8] = b"echo hi\r\nhi\r\n";
    const CLOCK: &str = "16:46:33";

    fn pane(cols: usize) -> tty59_vt::VtTerm {
        let mut term = tty59_vt::VtTerm::new(cols as u16, 12, 10_000);
        term.feed(PRIOR);
        term
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
    fn text(term: &mut tty59_vt::VtTerm) -> Vec<String> {
        let session = term.session();
        (session.topmost_line()..session.screen_lines() as i32)
            .map(|line| {
                (0..session.columns())
                    .map(|col| {
                        session
                            .glyph(GridPoint {
                                line,
                                col: col as u16,
                            })
                            .c
                    })
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

    /// The reported pattern: a clock prompt redrawn after each step of a drag
    /// that narrows the pane by more than one column at a time, then widens it
    /// back. Measured against the unguarded resize, so the test also proves it
    /// reproduces the fragments rather than passing on a scenario that never
    /// produced any.
    #[test]
    fn a_prompt_narrowed_under_its_right_side_leaves_no_fragments() {
        let drag = |guarded: bool| {
            let mut term = pane(98);
            term.feed(&zsh_redraw(98, 0, ""));
            for cols in [95, 92, 89, 86, 89, 92, 95, 98] {
                if guarded {
                    resize(&mut term.session(), TermSize::new(cols, 12), true);
                } else {
                    term.session().resize(cols, 12);
                }
                term.feed(&zsh_redraw(cols, 0, ""));
            }
            text(&mut term)
        };

        // The replaced engine stranded prompt fragments on a narrowing drag
        // — the bug the guard existed for. This engine's reflow carries the
        // cursor and soft wraps (an upstream 1.4.0 fix), so the guarded
        // shape is what holds now.
        assert_eq!(drag(true), ["echo hi", "hi", &prompt_row(98)]);
    }

    /// A command line long enough to wrap: zsh moves up by the rows it took at
    /// the old width, which the reflow changes (two rows at 98 columns, three
    /// at 60). Landing one row off either way strands a row of the old line or
    /// clears a row of the output above it.
    #[test]
    fn a_wrapped_command_line_is_redrawn_in_place() {
        let buffer = "x".repeat(150);
        let mut term = pane(98);
        term.feed(&zsh_redraw(98, 0, &buffer));

        resize(&mut term.session(), TermSize::new(60, 12), true);
        term.feed(&zsh_redraw(60, 1, &buffer));
        assert_eq!(
            text(&mut term),
            [
                "echo hi".to_string(),
                "hi".to_string(),
                format!("[~] {}", "x".repeat(56)),
                "x".repeat(60),
                "x".repeat(34),
            ]
        );

        resize(&mut term.session(), TermSize::new(98, 12), true);
        term.feed(&zsh_redraw(98, 2, &buffer));
        assert_eq!(
            text(&mut term),
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
        let mut term = pane(98);
        term.feed(&zsh_redraw(98, 0, ""));
        resize(&mut term.session(), TermSize::new(92, 12), true);
        resize(&mut term.session(), TermSize::new(86, 12), true);
        term.feed(&zsh_redraw(86, 0, ""));
        assert_eq!(text(&mut term), ["echo hi", "hi", &prompt_row(86)]);

        let buffer = "x".repeat(150);
        let mut term = pane(98);
        term.feed(&zsh_redraw(98, 0, &buffer));
        resize(&mut term.session(), TermSize::new(60, 12), true);
        resize(&mut term.session(), TermSize::new(80, 12), true);
        term.feed(&zsh_redraw(80, 1, &buffer));
        assert_eq!(
            text(&mut term),
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
        let mut term = tty59_vt::VtTerm::new(98, 4, 10_000);
        term.feed(b"one\r\ntwo\r\n");
        term.feed(&zsh_redraw(98, 0, &buffer));
        assert_eq!(i32::from(term.session().cursor().y), 3);

        resize(&mut term.session(), TermSize::new(60, 4), true);
        term.feed(&zsh_redraw(60, 1, &buffer));
        assert_eq!(
            text(&mut term),
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
        let mut term = pane(98);
        term.feed("y".repeat(150).as_bytes());
        resize(&mut term.session(), TermSize::new(60, 12), false);
        resize(&mut term.session(), TermSize::new(98, 12), false);
        assert_eq!(
            text(&mut term),
            [
                "echo hi".to_string(),
                "hi".to_string(),
                "y".repeat(98),
                "y".repeat(52)
            ]
        );

        let mut term = pane(98);
        term.feed(&zsh_redraw(98, 0, ""));
        resize(&mut term.session(), TermSize::new(98, 8), true);
        assert_eq!(text(&mut term), ["echo hi", "hi", &prompt_row(98)]);
    }

    /// The alternate screen belongs to a full-screen program, which repaints
    /// all of it; a stale prompt flag must not blank it.
    #[test]
    fn the_alternate_screen_is_left_alone() {
        let mut term = pane(60);
        term.feed(b"\x1b[?1049h\x1b[Hstatus bar");
        resize(&mut term.session(), TermSize::new(98, 12), true);
        assert_eq!(text(&mut term), ["status bar"]);
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
    fn dangling_output(cols: usize, guarded: bool) -> tty59_vt::VtTerm {
        let mut term = pane(cols);
        let mut marks = PromptBreak::default();
        term.feed(b"xyz");
        if guarded {
            marks.apply(&mut term.session(), PromptMark::Opened);
        }
        term.feed(&prompt_sp(cols));
        term.feed(&zsh_redraw(cols, 0, ""));
        if guarded {
            marks.apply(&mut term.session(), PromptMark::Ready);
        }
        term
    }

    /// `PROMPT_SP` wraps the dangling output's row into the prompt's. Taken
    /// for the prompt's own first row, that output was cleared by the next
    /// resize at the prompt — `printf xyz` simply vanished when a panel
    /// opened. Measured against the stream without the marks' fix, so the
    /// test also proves the scenario loses the output there.
    #[test]
    fn output_without_a_newline_survives_a_resize_at_the_prompt() {
        let narrow = |guarded: bool| {
            let mut term = dangling_output(98, guarded);
            resize(&mut term.session(), TermSize::new(60, 12), true);
            term.feed(&zsh_redraw(60, 0, ""));
            text(&mut term)
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
        let mut term = dangling_output(60, true);
        resize(&mut term.session(), TermSize::new(98, 12), true);
        term.feed(&zsh_redraw(98, 0, ""));
        assert_eq!(text(&mut term), ["echo hi", "hi", "xyz%", &prompt_row(98)]);
    }

    /// A prompt that carries straight on from the output — bash has no
    /// `PROMPT_SP` — wraps on its own text, not on padding, and that wrap is
    /// the shell's to count.
    #[test]
    fn a_prompt_continuing_the_output_row_keeps_its_wrap() {
        let mut term = pane(20);
        let mut marks = PromptBreak::default();
        term.feed(b"xyz");
        marks.apply(&mut term.session(), PromptMark::Opened);
        term.feed(b"user@host:~/some/long/path$ ");
        marks.apply(&mut term.session(), PromptMark::Ready);
        let row = i32::from(term.session().cursor().y) - 1;
        assert!(term.session().row_wraps(row));
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
