//! Copy rejoins the rows a program wrapped itself. Claude Code (Ink) wraps at
//! the pane's width by writing CRLF plus the block's indent, so the grid never
//! sets WRAPLINE on those rows and a plain copy keeps every break. The rule
//! was measured on 16 captures of Claude panes; `fixtures/` holds three.

use tty7_vt::model::{CellWidth, GridPoint, SelectionKind};
use tty7_vt::VtTermSession;
use unicode_width::UnicodeWidthChar;

/// One line as the copy sees it: the grid's rows up to the next one without
/// WRAPLINE, read from column 0 whatever the selection's start.
struct Row {
    text: String,
    /// Where the last physical row's text ends, in cells.
    end: usize,
}

/// The selection's text with app-wrapped rows joined. Any doubt (block
/// selection, a row count that doesn't line up) copies raw.
pub(super) fn joined(term: &VtTermSession) -> Option<String> {
    let raw = term.selection_to_string()?;
    let Some(selection) = term.selection() else {
        return Some(raw);
    };
    if selection.kind == SelectionKind::Block {
        return Some(raw);
    }
    let rows = grid_rows(term, selection.span.start.line, selection.span.end.line);
    Some(rejoin(&raw, &rows, term.columns()))
}

/// `raw` split at its newlines, which must be one per row boundary.
fn rejoin(raw: &str, rows: &[Row], cols: usize) -> String {
    let mut parts: Vec<&str> = raw.split('\n').collect();
    // A Lines selection ends in a newline its rows don't account for.
    let tail = if parts.len() == rows.len() + 1 && parts.last() == Some(&"") {
        parts.pop();
        "\n"
    } else {
        ""
    };
    if parts.len() != rows.len() {
        return raw.to_string();
    }
    apply(&parts, &joins(rows, cols)) + tail
}

fn grid_rows(term: &VtTermSession, first: i32, last: i32) -> Vec<Row> {
    let cols = term.columns();
    let mut rows = Vec::new();
    let mut text = String::new();
    let mut line = first;
    while line <= last {
        text += &term.text(
            GridPoint { line, col: 0 },
            GridPoint { line, col: (cols - 1) as u16 },
        );
        let wraps = term.row_wraps(line);
        if !wraps || line == last {
            let end = (0..cols)
                .rev()
                .find(|&c| {
                    let glyph = term.glyph(GridPoint { line, col: c as u16 });
                    glyph.c != ' ' && glyph.wide != CellWidth::SpacerHead
                })
                .map_or(0, |c| {
                    c + 1 + (term.glyph(GridPoint { line, col: c as u16 }).wide == CellWidth::Wide)
                        as usize
                });
            rows.push(Row {
                text: std::mem::take(&mut text).trim_end_matches('\n').to_string(),
                end,
            });
        }
        line += 1;
    }
    rows
}

/// What goes between two rows.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Sep {
    Newline,
    Space,
    /// Ink broke inside a word: a row that is one word, or wide (CJK) text,
    /// which wraps between any two characters.
    Nothing,
}

/// Where the rows before a boundary leave off.
#[derive(Default)]
struct Block {
    /// (leading spaces, text column) of the row that opened the current run.
    opened: Option<(usize, usize)>,
    fenced: bool,
    /// The indent of a `⎿` tool result whose rows we are inside.
    tool: Option<usize>,
}

impl Block {
    fn advance(&mut self, row: &Row) {
        let t = row.text.trim();
        if t.starts_with("```") {
            self.fenced = !self.fenced;
        }
        if !t.is_empty() {
            let indent = lead(&row.text);
            if self.tool.is_some_and(|t| indent <= t) {
                self.tool = None;
            }
            if t.starts_with('⎿') {
                self.tool = Some(indent);
            }
        }
        self.opened
            .get_or_insert_with(|| (lead(&row.text), text_col(&row.text)));
    }
}

/// For each boundary between `rows[i]` and `rows[i + 1]`, what goes there.
fn joins(rows: &[Row], cols: usize) -> Vec<Sep> {
    let mut block = Block::default();
    rows.windows(2)
        .map(|pair| {
            block.advance(&pair[0]);
            let sep = sep(&pair[0], &pair[1], &block, cols);
            if sep == Sep::Newline {
                block.opened = None;
            }
            sep
        })
        .collect()
}

fn sep(a: &Row, b: &Row, block: &Block, cols: usize) -> Sep {
    if block.fenced || block.tool.is_some() {
        return Sep::Newline;
    }
    let (a_trim, b_trim) = (a.text.trim(), b.text.trim());
    let Some(first) = b_trim.chars().next() else {
        return Sep::Newline;
    };
    if !(first.is_ascii() || first.is_alphabetic())
        || a_trim.ends_with('…')
        || structural(&a.text)
        || structural(&b.text)
    {
        return Sep::Newline;
    }
    let indent = lead(&b.text);
    let continues = block
        .opened
        .is_some_and(|(lo, hi)| (lo..=hi).contains(&indent));
    if !continues || text_col(&b.text) != indent {
        return Sep::Newline;
    }
    let word = cells(b_trim.split(' ').next().unwrap_or(""));
    // Ink breaks one cell early when the next word would land exactly on
    // the edge, so a word that just fits still counts as not fitting.
    // A word too long for any row tells nothing about where `a` ended.
    if a.end + 1 + word < cols || indent + word > cols {
        return Sep::Newline;
    }
    let wide = |c: Option<char>| c.is_some_and(|c| c.width() == Some(2));
    // A one-word row that fills the row is a word Ink cut in two: its hard
    // breaks fill to the edge, its word wraps stop short of it.
    let one_word = a.end >= cols && !a.text[text_byte(&a.text)..].trim_end().contains(' ');
    if one_word || (wide(a_trim.chars().last()) && wide(Some(first))) {
        Sep::Nothing
    } else {
        Sep::Space
    }
}

fn apply(parts: &[&str], seps: &[Sep]) -> String {
    let mut out = parts[0].to_string();
    for (part, &sep) in parts[1..].iter().zip(seps) {
        if sep == Sep::Newline {
            out.push('\n');
            out.push_str(part);
            continue;
        }
        out.truncate(out.trim_end().len());
        if sep == Sep::Space {
            out.push(' ');
        }
        out.push_str(part.trim_start());
    }
    out
}

/// Cells `text` takes in the grid, which sizes each char on its own: VS16
/// adds nothing, so `⚠️` is one cell, not the two `str` width gives it.
fn cells(text: &str) -> usize {
    text.chars().map(|c| c.width().unwrap_or(0)).sum()
}

fn lead(text: &str) -> usize {
    text.len() - text.trim_start_matches(' ').len()
}

/// Where a row's text starts once a list or block marker is skipped: `- `,
/// `1. `, or a run of symbols and a space (`⏺ `, `⚠️ `, `◯ `).
fn text_col(text: &str) -> usize {
    let at = text_byte(text);
    lead(text) + cells(&text[lead(text)..at])
}

/// The byte where a row's text starts, past its indent and any marker.
fn text_byte(text: &str) -> usize {
    let indent = lead(text);
    let rest = &text[indent..];
    let Some((marker, _)) = rest.split_once(' ') else {
        return indent;
    };
    let digits = marker.trim_end_matches(['.', ')']);
    let is_marker = matches!(marker, "-" | "*" | "+" | "•")
        || (marker.len() > digits.len()
            && (1..=3).contains(&digits.len())
            && digits.bytes().all(|b| b.is_ascii_digit()))
        || (!marker.is_empty()
            && marker
                .chars()
                .all(|c| !c.is_ascii() && !c.is_alphanumeric()));
    if is_marker {
        indent + marker.len() + 1
    } else {
        indent
    }
}

/// Box drawing, block elements, a raw table row, a prompt or a code fence:
/// never joined.
fn structural(text: &str) -> bool {
    let t = text.trim();
    text.chars().any(|c| ('\u{2500}'..='\u{259f}').contains(&c))
        || ["|", "$ ", "> ", "❯ ", "```"]
            .iter()
            .any(|p| t.starts_with(p))
}

#[cfg(test)]
mod tests {
    use super::*;

    use tty7_vt::model::Side;

    fn term(cols: usize, lines: usize, input: &str) -> tty7_vt::VtTerm {
        let mut term = tty7_vt::VtTerm::new(cols as u16, lines as u16, 10_000);
        term.feed(input.as_bytes());
        term
    }

    fn select(
        term: &mut tty7_vt::VtTerm,
        ty: SelectionKind,
        from: (i32, usize),
        to: (i32, usize),
    ) {
        term.session().set_selection(
            ty,
            GridPoint { line: from.0, col: from.1 as u16 },
            Side::Left,
            GridPoint { line: to.0, col: to.1 as u16 },
            Side::Right,
        );
    }

    /// `text` printed into a `cols`-wide pane and copied whole. A line longer
    /// than the pane soft-wraps, as `tty7 capture --plain` saw it.
    fn unwrap(text: &str, cols: usize) -> String {
        let lines: usize = text
            .split('\n')
            .map(|l| cells(l).div_ceil(cols).max(1))
            .sum();
        let mut term = term(cols, lines, &text.replace('\n', "\r\n"));
        select(
            &mut term,
            SelectionKind::Simple,
            (0, 0),
            (lines as i32 - 1, cols - 1),
        );
        joined(&term.session()).unwrap()
    }

    const C111: &str = include_str!("fixtures/copy_unwrap_111.txt");
    const C172: &str = include_str!("fixtures/copy_unwrap_172.txt");
    const C_CODE: &str = include_str!("fixtures/copy_unwrap_code.txt");

    fn has_line(out: &str, line: &str) -> bool {
        out.lines().any(|l| l == line)
    }

    #[test]
    fn wrapped_prose_joins() {
        let out = unwrap(C111, 111);
        assert!(has_line(
            &out,
            "⏺ #84 is now based on the new patch-stack main-niu, and GitHub reports it can merge cleanly. Its code is the same as what passed review: the only change is that its notes moved into the hotkey-window feature page. Its tests pass locally."
        ));
        assert!(has_line(
            &out,
            "※ recap: The goal is for tty7 to show the hotkey window on launch, instead of creating a new workspace, when it's your only workspace. That's PR #84, which passed review and is waiting on CI. Next, it merges and tty7 reloads on its own, and I'll check your sessions survive. (disable recaps in /config)"
        ));
        // A system line Claude continues at column 0.
        assert!(has_line(
            &out,
            "⏺ Background command \"Merge #84 once CI passes (network-error tolerant), then reload tty7\" completed (exit code 0)"
        ));
        let out = unwrap(C172, 172);
        assert!(has_line(
            &out,
            "※ recap: Goal: clean up shuck findings in your dotfiles, most-used first; the shell startup files are done and committed locally. Next, pick Z1, Z2 or Z3 for the no-op `unset GLOBAL_RCS` line, and say \"push\" for the 19 unpushed commits. (disable recaps in /config)"
        ));
        // `&&/||` in prose is not a table.
        assert!(out.contains(
            "fixing &&/|| logic into proper if/else, correcting a comment-continuation bug, adding"
        ));
    }

    #[test]
    fn the_reported_paragraph_joins() {
        let rows = "⏺ If the Theme field won't change or the popup comes back after they pick it, send me a screenshot. That would\n  be a bug.";
        assert_eq!(
            unwrap(rows, 111),
            "⏺ If the Theme field won't change or the popup comes back after they pick it, send me a screenshot. That would be a bug."
        );
    }

    #[test]
    fn list_items_keep_their_newlines() {
        let out = unwrap(C111, 111);
        assert!(out.contains("  Here's how to try it:\n  1. Close every workspace except the hotkey one.\n  2. Quit tty7 and open it again.\n"));
        let bullets = "  - Rule location: the rule is .claude/rules/app-layout.md, plus its Cursor copy and a pointer from the backend\n    skill.\n  - What it says: nothing new goes under <app>/services/, and other apps import only <app>.public and\n    <app>.models.";
        assert_eq!(
            unwrap(bullets, 111),
            "  - Rule location: the rule is .claude/rules/app-layout.md, plus its Cursor copy and a pointer from the backend skill.\n  - What it says: nothing new goes under <app>/services/, and other apps import only <app>.public and <app>.models."
        );
    }

    #[test]
    fn code_tables_and_tool_output_never_join() {
        let out = unwrap(C111, 111);
        // A box drawn round a summary.
        assert!(out.contains("  │  joins when ... the line runs to the right edge (the next word wouldn't\n  │                 have fit)"));
        // A tool call's command and its char-wrapped output stay as the grid has them.
        assert!(out.contains("tasks/befky\n      62qi.output;"));
        assert!(out.contains(
            "https://github.com/NorthIsUp/tty7/actions/runs/369394730\n     36/job/110627346251"
        ));
        assert!(out.contains("panicked at crates/tty7\n     -core/src/host/server.rs"));
        let fenced = "```\nlet joined = rows.iter().map(|r| r.text.as_str()).collect::<Vec<_>>().join(\" \"); // a long line of code\nthat continues\n```";
        let code_width = cells(fenced.lines().nth(1).unwrap());
        assert_eq!(unwrap(fenced, code_width), fenced);
        let table = "| a long table cell that runs right up to the edge of the pane, wider than the |\nrest |";
        assert_eq!(unwrap(table, 80), table);
    }

    #[test]
    fn a_short_row_before_an_indented_row_does_not_join() {
        let out = unwrap(C111, 111);
        assert!(out.contains("  Here's how to try it:\n  1."));
        let rows = "  Still open on your side: rotate the key.\n  Then rerun the push.";
        assert_eq!(unwrap(rows, 111), rows);
    }

    #[test]
    fn a_narrow_pane_joins_the_same() {
        // The reported paragraph and a list item, as Ink wraps them at 52 columns.
        let rows = "⏺ If the Theme field won't change or the popup comes\n  back after they pick it, send me a screenshot.\n  That would be a bug.\n\n  1. Close every workspace except the hotkey one,\n     then quit tty7 and open it again.\n  2. Quit tty7.";
        assert_eq!(
            unwrap(rows, 52),
            "⏺ If the Theme field won't change or the popup comes back after they pick it, send me a screenshot. That would be a bug.\n\n  1. Close every workspace except the hotkey one, then quit tty7 and open it again.\n  2. Quit tty7."
        );
    }

    #[test]
    fn the_grid_selection_joins_from_a_mid_row_start() {
        let soft = "a".repeat(45);
        let mut term = term(
            40,
            6,
            &format!(
                "⏺ If the Theme field won't change or\r\n  the popup comes back after they pick\r\n  it.\r\n{soft}"
            ),
        );
        select(&mut term, SelectionKind::Simple, (0, 2), (4, 39));
        assert_eq!(
            joined(&term.session()).unwrap(),
            format!(
                "If the Theme field won't change or the popup comes back after they pick it.\n{soft}"
            )
        );
    }

    #[test]
    fn wide_text_joins_without_a_space() {
        // Ten CJK chars fill 20 cells; Ink wraps between any two.
        let mut term = term(20, 3, "这是一个很长的中文句\r\n继续写完。");
        select(&mut term, SelectionKind::Simple, (0, 0), (1, 19));
        assert_eq!(joined(&term.session()).unwrap(), "这是一个很长的中文句继续写完。");
    }

    #[test]
    fn wide_on_one_side_only_keeps_the_space() {
        let emoji = format!("  {} 🎉\n  Next steps", "w".repeat(36));
        assert_eq!(
            unwrap(&emoji, 42),
            format!("  {} 🎉 Next steps", "w".repeat(36))
        );
        let mixed = format!("  {} 詳しくは\n  README を", "w".repeat(30));
        assert_eq!(
            unwrap(&mixed, 42),
            format!("  {} 詳しくは README を", "w".repeat(30))
        );
    }

    #[test]
    fn a_block_selection_copies_raw() {
        let mut term = term(
            40,
            3,
            "⏺ If the Theme field won't change or\r\n  the popup comes back",
        );
        select(&mut term, SelectionKind::Block, (0, 0), (1, 39));
        assert_eq!(joined(&term.session()), term.session().selection_to_string());
        assert!(joined(&term.session()).unwrap().contains("or\n"));
    }

    #[test]
    fn a_lines_selection_keeps_its_trailing_newline() {
        let mut term = term(
            40,
            3,
            "⏺ If the Theme field won't change or\r\n  the popup comes back",
        );
        select(&mut term, SelectionKind::Lines, (0, 0), (1, 0));
        assert_eq!(
            joined(&term.session()).unwrap(),
            "⏺ If the Theme field won't change or the popup comes back\n"
        );
    }

    #[test]
    fn rows_that_dont_line_up_with_the_text_copy_raw() {
        let rows = [Row {
            text: "x".repeat(40),
            end: 40,
        }];
        let raw = format!("{}\nmore", "x".repeat(40));
        assert_eq!(rejoin(&raw, &rows, 40), raw);
    }

    #[test]
    fn a_one_word_row_at_the_edge_joins_without_a_space() {
        let url = "https://github.com/NorthIsUp/tty7/actions/runs/36939473036";
        let (head, tail) = url.split_at(40);
        let rows = format!("  {head}\n  {tail} is the run.");
        assert_eq!(unwrap(&rows, 42), format!("  {url} is the run."));
    }

    #[test]
    fn a_vs16_emoji_is_one_cell() {
        assert_eq!(cells("⚠️"), 1);
        // 2 + "⚠️ " (2 cells) + 37 = 41 cells: the next word would not fit at 44.
        let rows = format!("  ⚠️ {}\n  more words", "w".repeat(37));
        assert_eq!(
            unwrap(&rows, 44),
            format!("  ⚠️ {} more words", "w".repeat(37))
        );
        assert_eq!(unwrap(&rows, 48), rows);
    }

    #[test]
    fn a_wrapped_code_line_joins_and_separate_code_lines_dont() {
        let out = unwrap(C_CODE, 111);
        assert!(has_line(
            &out,
            "  ! themes env version history sampleorg/tty/testing | head -1; cd ~/src/sampleorg/Sample_1/.worktrees/sm-12345-tty-set-run/src/sampl && .venv/bin/python ~/tmp/set_sync_testing.py --apply"
        ));
        assert!(out.contains("  ! cd /Users/adam/src/tty7-mdfix && mise run reload\n"));
        assert!(out.contains(
            "  # themes/palette.py: drop `import random`\n  from colour.utils.random import get_random_string\n  ...\n"
        ));
    }

    #[test]
    fn the_status_bar_never_joins() {
        let rows = "  #14 #15 #16 #17 #18 #19 #20 #21 #24 #25 #27 #28 #29 #30 #31 #32 #33 #34 #35 #36 #37 #38 #39 #40 #41 #42 …\n  ⏵⏵ auto mode on (shift+tab to cycle) · ← 19 agents";
        assert_eq!(unwrap(rows, 111), rows);
    }
}
