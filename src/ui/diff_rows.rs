//! Turning a parsed hunk into rows a diff view can lay out.
//!
//! Side-by-side and unified are two renderings of the same `Vec<DiffLine>`, so
//! the pairing logic lives here — outside either renderer — and is unit tested
//! without a window.

use crate::core::config::DiffViewMode;
use crate::terminal::git_diff::{DiffLine, Hunk, LineKind};

/// A tab is worth this many columns. Not configurable: a diff is read next to
/// the file's other lines, not on its own, and the grid has to line up.
const TAB_WIDTH: usize = 4;

/// Diff text is laid out as a single run, so a literal tab would advance to the
/// renderer's idea of a tab stop rather than the file's. Both views expand
/// them the same way, or the two halves of a split row would drift apart.
pub(crate) fn expand_tabs(text: &str) -> String {
    text.replace('\t', &" ".repeat(TAB_WIDTH))
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Side {
    Old,
    New,
}

pub(crate) struct SplitCell {
    pub(crate) no: Option<u32>,
    pub(crate) text: String,
    pub(crate) changed: bool,
    /// Which of the hunk's own lines this cell draws. `text` is the tab-
    /// expanded copy the grid needs; a copy to the clipboard has to reach past
    /// it to the line as the file wrote it.
    pub(crate) line: usize,
}

pub(crate) struct SplitRow {
    pub(crate) left: Option<SplitCell>,
    pub(crate) right: Option<SplitCell>,
}

/// Pairs each run of removals with the run of additions that follows it, so a
/// rewritten line sits opposite the line it replaced. Whichever run is shorter
/// leaves empty cells at the bottom of the pair.
pub(crate) fn split_hunk(lines: &[DiffLine]) -> Vec<SplitRow> {
    fn flush(
        rows: &mut Vec<SplitRow>,
        rem: &mut Vec<(usize, &DiffLine)>,
        add: &mut Vec<(usize, &DiffLine)>,
    ) {
        for i in 0..rem.len().max(add.len()) {
            rows.push(SplitRow {
                left: rem.get(i).map(|(idx, l)| SplitCell {
                    no: l.old_no,
                    text: expand_tabs(&l.text),
                    changed: true,
                    line: *idx,
                }),
                right: add.get(i).map(|(idx, l)| SplitCell {
                    no: l.new_no,
                    text: expand_tabs(&l.text),
                    changed: true,
                    line: *idx,
                }),
            });
        }
        rem.clear();
        add.clear();
    }

    let mut rows = Vec::new();
    let mut rem: Vec<(usize, &DiffLine)> = Vec::new();
    let mut add: Vec<(usize, &DiffLine)> = Vec::new();
    for (idx, line) in lines.iter().enumerate() {
        match line.kind {
            LineKind::Removed => rem.push((idx, line)),
            LineKind::Added => add.push((idx, line)),
            LineKind::Context => {
                flush(&mut rows, &mut rem, &mut add);
                rows.push(SplitRow {
                    left: Some(SplitCell {
                        no: line.old_no,
                        text: expand_tabs(&line.text),
                        changed: false,
                        line: idx,
                    }),
                    right: Some(SplitCell {
                        no: line.new_no,
                        text: expand_tabs(&line.text),
                        changed: false,
                        line: idx,
                    }),
                });
            }
        }
    }
    flush(&mut rows, &mut rem, &mut add);
    rows
}

pub(crate) struct UnifiedRow {
    pub(crate) old: Option<u32>,
    pub(crate) new: Option<u32>,
    pub(crate) kind: LineKind,
    pub(crate) text: String,
    /// Which of the hunk's own lines this row draws — the same reach past
    /// `text` a [`SplitCell`] needs, and one row is one line here.
    pub(crate) line: usize,
}

/// One row per line, in git's own order — every removal in a run first, then
/// every addition. That is the opposite of [`split_hunk`], and it is the whole
/// difference between the two views: unified shows the patch as it was written,
/// side-by-side re-pairs it into before and after.
pub(crate) fn unified_rows(lines: &[DiffLine]) -> Vec<UnifiedRow> {
    lines
        .iter()
        .enumerate()
        .map(|(idx, line)| UnifiedRow {
            old: line.old_no,
            new: line.new_no,
            kind: line.kind,
            text: expand_tabs(&line.text),
            line: idx,
        })
        .collect()
}

/// One hunk, already turned into whichever kind of row the current view draws.
pub(crate) enum HunkRows {
    Split(Vec<SplitRow>),
    Unified(Vec<UnifiedRow>),
}

impl HunkRows {
    pub(crate) fn build(mode: DiffViewMode, lines: &[DiffLine]) -> Self {
        match mode {
            DiffViewMode::Split => Self::Split(split_hunk(lines)),
            DiffViewMode::Unified => Self::Unified(unified_rows(lines)),
        }
    }

    pub(crate) fn len(&self) -> usize {
        match self {
            Self::Split(rows) => rows.len(),
            Self::Unified(rows) => rows.len(),
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Where a row sits in a file's card: which hunk, and which row inside it.
///
/// Ordered the way the rows are drawn, so a drag is nothing more than the
/// range between the two of these the pointer touched.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub(crate) struct RowId {
    pub(crate) hunk: usize,
    pub(crate) row: usize,
}

/// The rows a drag has run over, in one file.
#[derive(Clone, Debug)]
pub(crate) struct DiffSelection {
    /// The file the drag started in. A selection never spans two cards: two
    /// files are two documents, and a range across them would copy code from
    /// one into the middle of another.
    pub(crate) path: String,
    /// The view the rows were drawn in when the drag happened. The two views
    /// pair the same lines into different rows, so a selection means nothing
    /// in the other one — the overlay drops it when the mode changes rather
    /// than pretending to translate it.
    pub(crate) mode: DiffViewMode,
    /// Which column of the split view the drag started in; that column alone
    /// is copied, so dragging down the left gets the code as it was and down
    /// the right gets it as it is. `None` in the unified view, where a row
    /// *is* the line.
    pub(crate) side: Option<Side>,
    pub(crate) anchor: RowId,
    pub(crate) head: RowId,
}

impl DiffSelection {
    fn range(&self) -> (RowId, RowId) {
        match self.anchor <= self.head {
            true => (self.anchor, self.head),
            false => (self.head, self.anchor),
        }
    }

    /// Whether the selection covers this row. `side` names the column a split
    /// cell sits in, and is `None` for a unified row — a selection made in one
    /// column never lights up the other.
    pub(crate) fn covers(&self, path: &str, id: RowId, side: Option<Side>) -> bool {
        if self.path != path || self.side != side {
            return false;
        }
        let (start, end) = self.range();
        start <= id && id <= end
    }

    /// The code the drag ran over, spelled the way the file spells it.
    ///
    /// No `+`/`−` marker and no line numbers: what lands on the clipboard has
    /// to compile when it is pasted back, and the gutter is the diff talking
    /// about the code rather than the code itself. Tabs are left alone for the
    /// same reason — [`expand_tabs`] is how the grid draws a tab, not how the
    /// file stores one, and four spaces pasted into a tab-indented file is a
    /// whitespace bug the copier did not ask for.
    ///
    /// A split row with nothing on the selected side contributes nothing: the
    /// blank half of a pair is padding the layout invented, not an empty line
    /// in the file.
    pub(crate) fn text(&self, hunks: &[Hunk]) -> String {
        let (start, end) = self.range();
        let mut out: Vec<&str> = Vec::new();
        for (h, hunk) in hunks.iter().enumerate() {
            if h < start.hunk || h > end.hunk {
                continue;
            }
            let first = if h == start.hunk { start.row } else { 0 };
            let last = if h == end.hunk { end.row } else { usize::MAX };
            let mut push = |line: usize| {
                if let Some(l) = hunk.lines.get(line) {
                    out.push(&l.text);
                }
            };
            match HunkRows::build(self.mode, &hunk.lines) {
                HunkRows::Split(rows) => {
                    for row in rows.iter().take(last.saturating_add(1)).skip(first) {
                        let cell = match self.side.unwrap_or(Side::New) {
                            Side::Old => row.left.as_ref(),
                            Side::New => row.right.as_ref(),
                        };
                        if let Some(cell) = cell {
                            push(cell.line);
                        }
                    }
                }
                HunkRows::Unified(rows) => {
                    for row in rows.iter().take(last.saturating_add(1)).skip(first) {
                        push(row.line);
                    }
                }
            }
        }
        out.join(
            "
",
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(kind: LineKind, old: Option<u32>, new: Option<u32>, text: &str) -> DiffLine {
        DiffLine {
            kind,
            old_no: old,
            new_no: new,
            text: text.to_string(),
        }
    }

    /// A context line, two removals, one addition, a context line — the shape
    /// where the two views visibly disagree.
    fn hunk() -> Vec<DiffLine> {
        vec![
            line(LineKind::Context, Some(1), Some(1), "a"),
            line(LineKind::Removed, Some(2), None, "b"),
            line(LineKind::Removed, Some(3), None, "c"),
            line(LineKind::Added, None, Some(2), "B"),
            line(LineKind::Context, Some(4), Some(3), "d"),
        ]
    }

    #[test]
    fn pairs_removed_and_added_side_by_side() {
        let rows = split_hunk(&hunk());
        assert_eq!(rows.len(), 4);

        let l = rows[0].left.as_ref().unwrap();
        let r = rows[0].right.as_ref().unwrap();
        assert_eq!((l.no, l.text.as_str(), l.changed), (Some(1), "a", false));
        assert_eq!((r.no, r.text.as_str(), r.changed), (Some(1), "a", false));

        let l = rows[1].left.as_ref().unwrap();
        let r = rows[1].right.as_ref().unwrap();
        assert_eq!((l.no, l.text.as_str(), l.changed), (Some(2), "b", true));
        assert_eq!((r.no, r.text.as_str(), r.changed), (Some(2), "B", true));

        assert_eq!(rows[2].left.as_ref().unwrap().text, "c");
        assert!(rows[2].right.is_none());

        assert_eq!(rows[3].left.as_ref().unwrap().no, Some(4));
        assert_eq!(rows[3].right.as_ref().unwrap().no, Some(3));
    }

    #[test]
    fn expands_tabs_in_cell_text() {
        let lines = vec![line(LineKind::Added, None, Some(1), "\tindented")];
        let rows = split_hunk(&lines);
        assert_eq!(rows[0].right.as_ref().unwrap().text, "    indented");
        assert!(rows[0].left.is_none());
    }

    #[test]
    fn expand_tabs_is_a_fixed_width_substitution() {
        assert_eq!(expand_tabs("plain"), "plain");
        assert_eq!(expand_tabs("\tone"), "    one");
        assert_eq!(expand_tabs("\t\ttwo"), "        two");
        assert_eq!(
            expand_tabs("a\tb"),
            "a    b",
            "a fixed width, not the next tab stop — the diff has no column grid"
        );
        assert_eq!(expand_tabs(""), "");
    }

    #[test]
    fn unified_keeps_gits_own_order() {
        let rows = unified_rows(&hunk());
        let shape: Vec<(Option<u32>, Option<u32>, LineKind, &str)> = rows
            .iter()
            .map(|r| (r.old, r.new, r.kind, r.text.as_str()))
            .collect();
        assert_eq!(
            shape,
            [
                (Some(1), Some(1), LineKind::Context, "a"),
                (Some(2), None, LineKind::Removed, "b"),
                (Some(3), None, LineKind::Removed, "c"),
                (None, Some(2), LineKind::Added, "B"),
                (Some(4), Some(3), LineKind::Context, "d"),
            ],
            "both removals come before the addition, unlike the split view"
        );
    }

    #[test]
    fn unified_numbers_each_column_from_the_side_it_belongs_to() {
        let rows = unified_rows(&hunk());
        assert!(
            rows.iter()
                .all(|r| (r.old.is_some() && r.new.is_some()) == (r.kind == LineKind::Context)),
            "a context line is the only kind that exists on both sides"
        );
        assert!(
            rows.iter()
                .filter(|r| r.kind == LineKind::Added)
                .all(|r| r.old.is_none())
        );
        assert!(
            rows.iter()
                .filter(|r| r.kind == LineKind::Removed)
                .all(|r| r.new.is_none())
        );
    }

    #[test]
    fn unified_expands_tabs_the_same_way_split_does() {
        let lines = vec![line(LineKind::Added, None, Some(1), "\tindented")];
        assert_eq!(unified_rows(&lines)[0].text, "    indented");
    }

    #[test]
    fn both_views_render_every_line_exactly_once() {
        let lines = hunk();
        let unified = unified_rows(&lines);
        assert_eq!(unified.len(), lines.len());

        let cells: usize = split_hunk(&lines)
            .iter()
            .map(|r| r.left.is_some() as usize + r.right.is_some() as usize)
            .sum();
        let context = lines.iter().filter(|l| l.kind == LineKind::Context).count();
        assert_eq!(
            cells,
            lines.len() + context,
            "a context line fills two cells, a change fills one"
        );
    }
    fn patch(lines: Vec<DiffLine>) -> Hunk {
        Hunk {
            header: "@@ -1,4 +1,3 @@".to_string(),
            lines,
        }
    }

    fn drag(
        mode: DiffViewMode,
        side: Option<Side>,
        anchor: (usize, usize),
        head: (usize, usize),
    ) -> DiffSelection {
        DiffSelection {
            path: "src/a.rs".to_string(),
            mode,
            side,
            anchor: RowId {
                hunk: anchor.0,
                row: anchor.1,
            },
            head: RowId {
                hunk: head.0,
                row: head.1,
            },
        }
    }

    #[test]
    fn a_drag_down_the_new_column_copies_the_file_as_it_now_reads() {
        let hunks = [patch(hunk())];
        let text = drag(DiffViewMode::Split, Some(Side::New), (0, 0), (0, 3)).text(&hunks);
        assert_eq!(
            text, "a\nB\nd",
            "the removed-only row is padding on this side, not an empty line"
        );
    }

    #[test]
    fn a_drag_down_the_old_column_copies_the_file_as_it_was() {
        let hunks = [patch(hunk())];
        let text = drag(DiffViewMode::Split, Some(Side::Old), (0, 0), (0, 3)).text(&hunks);
        assert_eq!(text, "a\nb\nc\nd");
    }

    #[test]
    fn the_unified_view_copies_the_rows_in_the_order_it_drew_them() {
        let hunks = [patch(hunk())];
        let text = drag(DiffViewMode::Unified, None, (0, 1), (0, 3)).text(&hunks);
        assert_eq!(
            text, "b\nc\nB",
            "both removals then the addition — the order on screen"
        );
    }

    #[test]
    fn a_drag_the_other_way_round_copies_the_same_rows() {
        let hunks = [patch(hunk())];
        let forwards = drag(DiffViewMode::Unified, None, (0, 1), (0, 3)).text(&hunks);
        let backwards = drag(DiffViewMode::Unified, None, (0, 3), (0, 1)).text(&hunks);
        assert_eq!(forwards, backwards);
    }

    #[test]
    fn one_row_copies_one_line() {
        let hunks = [patch(hunk())];
        assert_eq!(
            drag(DiffViewMode::Unified, None, (0, 2), (0, 2)).text(&hunks),
            "c"
        );
    }

    /// What lands on the clipboard has to compile when it is pasted back, so
    /// none of the diff's own furniture may ride along with it.
    #[test]
    fn nothing_the_gutter_draws_is_copied() {
        let lines = vec![
            line(LineKind::Removed, Some(9), None, "let old = 1;"),
            line(LineKind::Added, None, Some(9), "let new = 2;"),
        ];
        let hunks = [patch(lines)];
        for (mode, side) in [
            (DiffViewMode::Split, Some(Side::Old)),
            (DiffViewMode::Split, Some(Side::New)),
            (DiffViewMode::Unified, None),
        ] {
            let text = drag(mode, side, (0, 0), (0, 9)).text(&hunks);
            assert!(!text.contains('+'), "marker in {text:?}");
            assert!(!text.contains('−'), "marker in {text:?}");
            assert!(!text.contains('9'), "line number in {text:?}");
        }
    }

    /// [`expand_tabs`] is how the grid draws a tab, not how the file stores
    /// one. Copying the drawn text would paste four spaces into a tab-indented
    /// file — a whitespace change nobody asked for.
    #[test]
    fn copying_keeps_the_tabs_the_file_was_written_with() {
        let hunks = [patch(vec![line(
            LineKind::Added,
            None,
            Some(1),
            "\tindented",
        )])];
        assert_eq!(
            unified_rows(&hunks[0].lines)[0].text,
            "    indented",
            "drawn with the tab expanded"
        );
        assert_eq!(
            drag(DiffViewMode::Unified, None, (0, 0), (0, 0)).text(&hunks),
            "\tindented",
            "copied with the tab intact"
        );
    }

    #[test]
    fn a_drag_across_hunks_copies_every_row_between_its_ends() {
        let hunks = [
            patch(vec![
                line(LineKind::Context, Some(1), Some(1), "one"),
                line(LineKind::Context, Some(2), Some(2), "two"),
            ]),
            patch(vec![
                line(LineKind::Context, Some(9), Some(9), "nine"),
                line(LineKind::Context, Some(10), Some(10), "ten"),
            ]),
        ];
        let text = drag(DiffViewMode::Unified, None, (0, 1), (1, 0)).text(&hunks);
        assert_eq!(
            text, "two\nnine",
            "the tail of the first hunk and the head of the second, nothing else"
        );
        let all = drag(DiffViewMode::Split, Some(Side::New), (0, 0), (1, 1)).text(&hunks);
        assert_eq!(all, "one\ntwo\nnine\nten");
    }

    #[test]
    fn a_selection_lights_only_the_column_the_drag_started_in() {
        let sel = drag(DiffViewMode::Split, Some(Side::New), (0, 0), (0, 2));
        let inside = RowId { hunk: 0, row: 1 };
        assert!(sel.covers("src/a.rs", inside, Some(Side::New)));
        assert!(!sel.covers("src/a.rs", inside, Some(Side::Old)));
        assert!(
            !sel.covers("src/b.rs", inside, Some(Side::New)),
            "a selection belongs to one file's card"
        );
        assert!(!sel.covers("src/a.rs", RowId { hunk: 0, row: 3 }, Some(Side::New)));
        assert!(!sel.covers("src/a.rs", RowId { hunk: 1, row: 0 }, Some(Side::New)));
    }

    #[test]
    fn a_unified_selection_never_lights_a_split_cell() {
        let sel = drag(DiffViewMode::Unified, None, (0, 0), (0, 2));
        let inside = RowId { hunk: 0, row: 1 };
        assert!(sel.covers("src/a.rs", inside, None));
        assert!(!sel.covers("src/a.rs", inside, Some(Side::New)));
    }

    /// Rows are ordered the way they are drawn, so the range between two of
    /// them is exactly what the pointer crossed.
    #[test]
    fn rows_order_by_hunk_before_row() {
        assert!(RowId { hunk: 0, row: 9 } < RowId { hunk: 1, row: 0 });
        assert!(RowId { hunk: 1, row: 0 } < RowId { hunk: 1, row: 1 });
    }

    #[test]
    fn a_selection_pointing_past_the_patch_copies_nothing() {
        let hunks = [patch(hunk())];
        assert_eq!(
            drag(DiffViewMode::Unified, None, (4, 0), (4, 2)).text(&hunks),
            ""
        );
        assert_eq!(
            drag(DiffViewMode::Unified, None, (0, 0), (0, 2)).text(&[]),
            ""
        );
    }

    #[test]
    fn each_view_builds_the_rows_its_renderer_draws() {
        let lines = hunk();
        assert_eq!(HunkRows::build(DiffViewMode::Split, &lines).len(), 4);
        assert_eq!(HunkRows::build(DiffViewMode::Unified, &lines).len(), 5);
        assert!(HunkRows::build(DiffViewMode::Split, &[]).is_empty());
    }
}
