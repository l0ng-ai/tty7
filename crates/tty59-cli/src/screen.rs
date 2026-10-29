//! Turning a captured pane back into text.
//!
//! `tty59 capture` hands back what the daemon stored: the PTY's bytes, escapes
//! and all. Recovering the text from that is a terminal's job, not a regex's.
//! A stripper that deletes `ESC[`-sequences gets the easy 90% and then lies
//! about the rest, because the information it needs was never in the byte
//! stream to begin with — it is in the grid those bytes drive:
//!
//! - **Wrapping.** A shell writing 200 characters into a 203-column pane emits
//!   200 characters and no newline. At 120 columns the same bytes are two
//!   display rows of one logical line. Only the grid knows which, via
//!   `WRAPLINE`, and `bounds_to_string` honours it — so a wrapped line comes
//!   back joined instead of split at an invented newline.
//! - **Overwriting.** `\r` means "back to column 0", and what follows replaces
//!   what was there. A stripper can only guess (turning it into a newline, which
//!   is why a syntax-highlighting shell used to read as `eecho …echo`). Replayed
//!   through a grid, the cell simply holds the last thing written to it.
//! - **Cursor addressing.** `ESC[5;80H` puts text somewhere specific. Delete the
//!   escape and the text lands wherever the previous character left off.
//! - **Width.** A wide char owns two cells and its spacer must not become a
//!   second character; combining marks belong to the cell they modify.
//!
//! So this parses, through the `tty59-vt` seam onto the same engine and
//! rev the GUI renders panes with — meaning `capture --plain` and the
//! window agree about what a pane says. (This was the Ghostty migration's
//! first consumer; the differential test that proved the two engines read
//! the same pane retired with the engine it replaced — docs/
//! libghostty-migration.md.)

use tty59_core::daemon::protocol::WinSize;

use crate::backend::CaptureSegment;

/// Replay every segment through a grid of its own size and join the results.
///
/// Each segment gets its own terminal because a segment exists precisely
/// because the pane was a different size then; feeding them all to one grid
/// would re-wrap the older output at the newest width.
pub fn render(segments: &[CaptureSegment]) -> String {
    let mut out = String::new();
    for segment in segments {
        let text = render_segment(segment.size, &segment.bytes);
        if text.is_empty() {
            continue;
        }
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(&text);
    }
    out
}

/// One segment, replayed through the seam's engine (libghostty-vt since
/// Phase 6b; alacritty before it, answers frozen parity-equal).
fn render_segment(size: WinSize, bytes: &[u8]) -> String {
    // A zero-sized grid is the one thing the engine holds no meaning for,
    // and a server that never sent a size leaves us with whatever the
    // caller guessed — so clamp rather than trust.
    let cols = size.cols.max(1);
    let rows = size.rows.max(1);
    // The scrollback the grid keeps is for output this segment pushed off
    // its own screen: a segment can be far longer than one screenful, and
    // dropping what scrolled away would answer a different question than
    // the raw form does. 10k lines is the daemon ring's order of magnitude.
    // `tty59 capture` has no PTY to answer, so effects are dropped: the
    // facade's plain feed parses state changes only, which is everything a
    // capture replays.
    let mut term = tty59_vt::VtTerm::new(cols, rows, 10_000);
    term.feed(bytes);
    trim_blank_edges(&term.text_full())
}

/// Drop the blank lines a rectangle leaves at either end of the text.
///
/// A grid is a fixed rectangle, so the rows under the last line of output are
/// real blank cells — 20 of them after a shell prompt, which is padding, not
/// content. The top gets them too: `ESC[2J` scrolls the screen into history, so
/// a TUI that clears before drawing starts its capture with blank history lines.
///
/// Blank lines *between* output are content and stay. So does the indentation of
/// a line the app positioned — only whole empty rows at the edges go.
fn trim_blank_edges(text: &str) -> String {
    let lines: Vec<&str> = text.split('\n').collect();
    let blank = |line: &&str| line.trim().is_empty();
    let Some(first) = lines.iter().position(|l| !blank(l)) else {
        return String::new();
    };
    let last = lines
        .iter()
        .rposition(|l| !blank(l))
        .expect("a non-blank line was just found");
    lines[first..=last].join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn size(cols: u16, rows: u16) -> WinSize {
        WinSize {
            cols,
            rows,
            cell_w: 8,
            cell_h: 16,
        }
    }

    fn plain(cols: u16, rows: u16, bytes: &str) -> String {
        render(&[CaptureSegment {
            size: size(cols, rows),
            bytes: bytes.as_bytes().to_vec(),
        }])
    }

    #[test]
    fn colour_and_cursor_escapes_leave_no_trace() {
        let text = plain(40, 5, "\x1b[01;32mhello\x1b[0m \x1b[36mworld\x1b[00m\r\n");
        assert_eq!(text, "hello world");
    }

    #[test]
    fn osc_titles_and_shell_integration_marks_are_not_output() {
        // The prompt marks a real shell emits around every command. None of it
        // is text the user ever saw.
        let text = plain(
            40,
            5,
            "\x1b]2;thomas@box:/tmp\x07\x1b]133;A\x07$ \x1b]133;B\x07ls\r\n\
             \x1b]133;C\x07a.txt\r\n\x1b]133;D;0\x07",
        );
        assert_eq!(text, "$ ls\na.txt");
    }

    #[test]
    fn a_wrapped_line_comes_back_as_one_line() {
        // 30 characters into a 10-column pane: the terminal wrapped it across
        // three rows, but the shell wrote one line and that is what it is. A
        // stripper cannot tell this from three real lines.
        let text = plain(10, 6, &"x".repeat(30));
        assert_eq!(text, "x".repeat(30));
        assert!(!text.contains('\n'), "a wrap is not a newline: {text:?}");
    }

    #[test]
    fn a_real_newline_still_breaks_the_line() {
        let text = plain(10, 6, "one\r\ntwo\r\n");
        assert_eq!(text, "one\ntwo");
    }

    #[test]
    fn a_carriage_return_overwrites_instead_of_breaking() {
        // What a progress bar does. The user saw "100%", never "10%".
        let text = plain(20, 4, "10%\r50%\r100%\r\n");
        assert_eq!(text, "100%");
    }

    #[test]
    fn a_redrawn_line_reads_as_its_final_state() {
        // zsh-syntax-highlighting rewrites the line it is echoing, which is why
        // a regex stripper produces "eecho …echo". The grid holds one copy.
        let text = plain(30, 4, "echo hi\x1b[7D\x1b[32mecho\x1b[39m\x1b[3C\r\nhi\r\n");
        assert_eq!(text, "echo hi\nhi");
    }

    #[test]
    fn cursor_addressing_puts_text_where_it_was_addressed() {
        // A TUI drawing at absolute positions. Strip the escapes and the two
        // words collide; replay them and they are on different rows.
        let text = plain(20, 4, "\x1b[2J\x1b[1;1Htop\x1b[3;5Hdeep");
        assert_eq!(text, "top\n\n    deep");
    }

    #[test]
    fn wide_characters_keep_one_cell_pair_and_one_character() {
        let text = plain(20, 3, "宽宽 ok\r\n");
        assert_eq!(text, "宽宽 ok");
    }

    #[test]
    fn combining_marks_stay_with_the_cell_they_modify() {
        let text = plain(20, 3, "e\u{0301}cole\r\n");
        assert_eq!(text, "e\u{0301}cole");
    }

    #[test]
    fn the_blank_rows_under_the_output_are_dropped_but_gaps_are_kept() {
        let text = plain(20, 24, "first\r\n\r\nthird\r\n");
        assert_eq!(
            text, "first\n\nthird",
            "a blank line between output is content; the 20 rows after it are not"
        );
    }

    #[test]
    fn an_empty_capture_renders_to_nothing() {
        assert_eq!(plain(20, 5, ""), "");
        assert_eq!(render(&[]), "");
    }

    #[test]
    fn each_segment_is_replayed_at_its_own_width() {
        // The pane was 10 columns wide, then 40. Wrapping the older segment at
        // the newer width — or the reverse — would split or join the wrong line.
        let text = render(&[
            CaptureSegment {
                size: size(10, 4),
                bytes: "y".repeat(15).into_bytes(),
            },
            CaptureSegment {
                size: size(40, 4),
                bytes: b"after the resize\r\n".to_vec(),
            },
        ]);
        assert_eq!(text, format!("{}\nafter the resize", "y".repeat(15)));
    }

    #[test]
    fn invalid_utf8_does_not_derail_the_parse() {
        let text = render(&[CaptureSegment {
            size: size(20, 4),
            bytes: b"ok \xff\xfe done\r\n".to_vec(),
        }]);
        assert!(text.starts_with("ok "), "{text:?}");
        assert!(text.ends_with("done"), "{text:?}");
    }

}
