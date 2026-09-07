//! Turning a parsed hunk into rows a diff view can lay out.
//!
//! Side-by-side and unified are two renderings of the same `Vec<DiffLine>`, so
//! the pairing logic lives here — outside either renderer — and is unit tested
//! without a window.

use crate::terminal::git_diff::{DiffLine, LineKind};

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

#[derive(PartialEq, Eq)]
pub(crate) struct SplitCell {
    pub(crate) no: Option<u32>,
    pub(crate) text: String,
    pub(crate) changed: bool,
}

#[derive(PartialEq, Eq)]
pub(crate) struct SplitRow {
    pub(crate) left: Option<SplitCell>,
    pub(crate) right: Option<SplitCell>,
}

/// Pairs each run of removals with the run of additions that follows it, so a
/// rewritten line sits opposite the line it replaced. Whichever run is shorter
/// leaves empty cells at the bottom of the pair.
pub(crate) fn split_hunk(lines: &[DiffLine]) -> Vec<SplitRow> {
    fn flush(rows: &mut Vec<SplitRow>, rem: &mut Vec<&DiffLine>, add: &mut Vec<&DiffLine>) {
        for i in 0..rem.len().max(add.len()) {
            rows.push(SplitRow {
                left: rem.get(i).map(|l| SplitCell {
                    no: l.old_no,
                    text: expand_tabs(&l.text),
                    changed: true,
                }),
                right: add.get(i).map(|l| SplitCell {
                    no: l.new_no,
                    text: expand_tabs(&l.text),
                    changed: true,
                }),
            });
        }
        rem.clear();
        add.clear();
    }

    let mut rows = Vec::new();
    let mut rem: Vec<&DiffLine> = Vec::new();
    let mut add: Vec<&DiffLine> = Vec::new();
    for line in lines {
        match line.kind {
            LineKind::Removed => rem.push(line),
            LineKind::Added => add.push(line),
            LineKind::Context => {
                flush(&mut rows, &mut rem, &mut add);
                rows.push(SplitRow {
                    left: Some(SplitCell {
                        no: line.old_no,
                        text: expand_tabs(&line.text),
                        changed: false,
                    }),
                    right: Some(SplitCell {
                        no: line.new_no,
                        text: expand_tabs(&line.text),
                        changed: false,
                    }),
                });
            }
        }
    }
    flush(&mut rows, &mut rem, &mut add);
    rows
}

#[derive(PartialEq, Eq)]
pub(crate) struct UnifiedRow {
    pub(crate) old: Option<u32>,
    pub(crate) new: Option<u32>,
    pub(crate) kind: LineKind,
    pub(crate) text: String,
}

/// One row per line, in git's own order — every removal in a run first, then
/// every addition. That is the opposite of [`split_hunk`], and it is the whole
/// difference between the two views: unified shows the patch as it was written,
/// side-by-side re-pairs it into before and after.
pub(crate) fn unified_rows(lines: &[DiffLine]) -> Vec<UnifiedRow> {
    lines
        .iter()
        .map(|line| UnifiedRow {
            old: line.old_no,
            new: line.new_no,
            kind: line.kind,
            text: expand_tabs(&line.text),
        })
        .collect()
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
}
