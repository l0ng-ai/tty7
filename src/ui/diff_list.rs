//! Flattening a diff snapshot into the one-dimensional list of rows the
//! overlay scrolls.
//!
//! The overlay used to build its whole patch as a nested element tree —
//! a card per file, a hunk header and one element per line inside it, every
//! one of them constructed on every frame. A `20_000` line budget is around
//! `120_000` elements to lay out, and gpui notifies the view on each scroll
//! wheel event, so a diff of any size rebuilt the entire tree tens of times a
//! second.
//!
//! [`gpui::list`] only builds the rows it can see, but it is one-dimensional:
//! it takes an index, not a tree. So the tree is flattened here, once per
//! change to what is on screen.

use std::collections::HashMap;

use crate::core::config::DiffViewMode;
use crate::terminal::git_diff::{
    AUTO_COLLAPSE_LINES, DiffSnapshot, FileDiff, FileStatus, MAX_RENDERED_FILES, Truncation,
};
use crate::ui::diff_rows::{SplitRow, UnifiedRow, split_hunk, unified_rows};

/// Everything the file header row draws, lifted out of its [`FileDiff`].
///
/// A copy rather than a borrow: the row list outlives the frame that built it,
/// and these are a handful of small fields against a file's whole patch.
#[derive(PartialEq, Eq)]
pub(crate) struct FileHead {
    /// Position in `snap.files`, for the element id and nothing else.
    pub(crate) index: usize,
    pub(crate) path: String,
    /// `old → new` for a rename, the path itself otherwise.
    pub(crate) shown_path: String,
    pub(crate) status: FileStatus,
    pub(crate) added: u32,
    pub(crate) removed: u32,
    pub(crate) binary: bool,
    pub(crate) expandable: bool,
    pub(crate) expanded: bool,
}

#[derive(PartialEq, Eq)]
pub(crate) enum DiffRow {
    /// The space that used to be the `gap_3` of a flex column.
    Gap,
    /// The banner above an auto-collapsed tree. Its text is derived from the
    /// snapshot at render time, so nothing is carried here.
    Oversized,
    FileHeader(FileHead),
    HunkHeader {
        text: String,
        /// The first hunk of a file follows its header and needs no rule
        /// above it; every later one is separating itself from the lines of
        /// the hunk before.
        leads: bool,
    },
    Split(SplitRow),
    Unified(UnifiedRow),
    Truncated(Truncation),
    MoreFiles {
        rest: usize,
    },
    UntrackedHeader {
        total: usize,
    },
    Untracked {
        index: usize,
        path: String,
    },
    MoreUntracked {
        rest: usize,
    },
}

/// The rows of the whole overlay, in the order they scroll past.
pub(crate) fn build_rows(
    snap: &DiffSnapshot,
    expanded: &HashMap<String, bool>,
    focused: Option<usize>,
    mode: DiffViewMode,
    oversized: bool,
) -> Vec<DiffRow> {
    let mut out: Vec<DiffRow> = Vec::new();
    // Stands in for the gap between the groups this list used to be a flex
    // column of: gpui's list stacks its items with nothing between them.
    let gap = |out: &mut Vec<DiffRow>| {
        if !out.is_empty() {
            out.push(DiffRow::Gap);
        }
    };

    if oversized {
        out.push(DiffRow::Oversized);
    }

    let shown = snap.files.len().min(MAX_RENDERED_FILES);
    for (idx, file) in snap.files.iter().enumerate() {
        if focused.is_some_and(|f| f != idx) {
            continue;
        }
        if focused.is_none() && idx >= shown {
            break;
        }
        let is_expanded = if focused == Some(idx) {
            expanded.get(&file.path).copied().unwrap_or(true)
        } else {
            file_expanded(file, expanded, oversized)
        };
        gap(&mut out);
        out.extend(file_rows(idx, file, is_expanded, mode));
    }

    if focused.is_none() && snap.files.len() > shown {
        gap(&mut out);
        out.push(DiffRow::MoreFiles {
            rest: snap.files.len() - shown,
        });
    }

    if focused.is_none() && !snap.untracked.is_empty() {
        gap(&mut out);
        out.extend(untracked_rows(snap));
    }

    out
}

/// The single synthesized file an untracked file's preview shows.
///
/// `usize::MAX` keeps its element ids clear of the real list's.
pub(crate) fn preview_rows(file: &FileDiff, mode: DiffViewMode) -> Vec<DiffRow> {
    file_rows(usize::MAX, file, true, mode)
}

fn file_rows(index: usize, file: &FileDiff, expanded: bool, mode: DiffViewMode) -> Vec<DiffRow> {
    let expandable =
        !file.binary && (!file.hunks.is_empty() || file.truncated == Some(Truncation::Budget));
    let shown_path = match &file.old_path {
        Some(old) => format!("{old} → {}", file.path),
        None => file.path.clone(),
    };
    let mut rows = vec![DiffRow::FileHeader(FileHead {
        index,
        path: file.path.clone(),
        shown_path,
        status: file.status,
        added: file.added,
        removed: file.removed,
        binary: file.binary,
        expandable,
        expanded,
    })];

    if expanded && (!file.hunks.is_empty() || file.truncated.is_some()) {
        for (h, hunk) in file.hunks.iter().enumerate() {
            rows.push(DiffRow::HunkHeader {
                text: hunk.header.clone(),
                leads: h == 0,
            });
            match mode {
                DiffViewMode::Split => {
                    rows.extend(split_hunk(&hunk.lines).into_iter().map(DiffRow::Split))
                }
                DiffViewMode::Unified => {
                    rows.extend(unified_rows(&hunk.lines).into_iter().map(DiffRow::Unified))
                }
            }
        }
        if let Some(reason) = file.truncated {
            rows.push(DiffRow::Truncated(reason));
        }
    }

    rows
}

fn untracked_rows(snap: &DiffSnapshot) -> Vec<DiffRow> {
    let total = snap.untracked_count();
    let shown = &snap.untracked[..snap.untracked.len().min(MAX_RENDERED_FILES)];
    let mut rows = vec![DiffRow::UntrackedHeader { total }];
    for (index, path) in shown.iter().enumerate() {
        rows.push(DiffRow::Untracked {
            index,
            path: path.clone(),
        });
    }
    if total > shown.len() {
        rows.push(DiffRow::MoreUntracked {
            rest: total - shown.len(),
        });
    }
    rows
}

/// Whether a file's lines show on their own, absent an explicit choice.
pub(crate) fn file_expanded(
    file: &FileDiff,
    expanded: &HashMap<String, bool>,
    collapse_all: bool,
) -> bool {
    if let Some(&want) = expanded.get(&file.path) {
        return want;
    }
    !collapse_all && file.added + file.removed <= AUTO_COLLAPSE_LINES
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::git_diff::{DiffLine, Hunk, LineKind};

    fn line(kind: LineKind, old: Option<u32>, new: Option<u32>) -> DiffLine {
        DiffLine {
            kind,
            old_no: old,
            new_no: new,
            text: "x".to_string(),
        }
    }

    /// One removal answered by one addition, wrapped in a context line either
    /// side — four lines unified, three rows split.
    fn hunk() -> Hunk {
        Hunk {
            header: "@@ -1,3 +1,3 @@".to_string(),
            lines: vec![
                line(LineKind::Context, Some(1), Some(1)),
                line(LineKind::Removed, Some(2), None),
                line(LineKind::Added, None, Some(2)),
                line(LineKind::Context, Some(3), Some(3)),
            ],
        }
    }

    fn file(path: &str, hunks: usize) -> FileDiff {
        FileDiff {
            path: path.to_string(),
            old_path: None,
            status: FileStatus::Modified,
            added: 1,
            removed: 1,
            binary: false,
            truncated: None,
            hunks: std::iter::repeat_n(hunk(), hunks).collect(),
        }
    }

    fn snapshot(files: Vec<FileDiff>) -> DiffSnapshot {
        DiffSnapshot {
            files,
            ..Default::default()
        }
    }

    fn shape(rows: &[DiffRow]) -> Vec<&'static str> {
        rows.iter()
            .map(|row| match row {
                DiffRow::Gap => "gap",
                DiffRow::Oversized => "oversized",
                DiffRow::FileHeader(_) => "file",
                DiffRow::HunkHeader { .. } => "hunk",
                DiffRow::Split(_) => "split",
                DiffRow::Unified(_) => "unified",
                DiffRow::Truncated(_) => "truncated",
                DiffRow::MoreFiles { .. } => "more-files",
                DiffRow::UntrackedHeader { .. } => "untracked-header",
                DiffRow::Untracked { .. } => "untracked",
                DiffRow::MoreUntracked { .. } => "more-untracked",
            })
            .collect()
    }

    fn open(paths: [&str; 1]) -> HashMap<String, bool> {
        paths.into_iter().map(|p| (p.to_string(), true)).collect()
    }

    #[test]
    fn an_open_file_flattens_to_a_header_a_hunk_header_and_a_row_per_line() {
        let snap = snapshot(vec![file("a.rs", 1)]);
        let rows = build_rows(&snap, &HashMap::new(), None, DiffViewMode::Unified, false);
        assert_eq!(
            shape(&rows),
            ["file", "hunk", "unified", "unified", "unified", "unified"]
        );

        let rows = build_rows(&snap, &HashMap::new(), None, DiffViewMode::Split, false);
        assert_eq!(
            shape(&rows),
            ["file", "hunk", "split", "split", "split"],
            "the split view pairs the removal with the addition that replaced it"
        );
    }

    #[test]
    fn only_the_first_hunk_of_a_file_follows_its_header() {
        let rows = build_rows(
            &snapshot(vec![file("a.rs", 2)]),
            &HashMap::new(),
            None,
            DiffViewMode::Unified,
            false,
        );
        let leads: Vec<bool> = rows
            .iter()
            .filter_map(|r| match r {
                DiffRow::HunkHeader { leads, .. } => Some(*leads),
                _ => None,
            })
            .collect();
        assert_eq!(
            leads,
            [true, false],
            "the second hunk draws the rule that separates it from the first"
        );
    }

    #[test]
    fn a_collapsed_file_is_a_single_row() {
        let snap = snapshot(vec![file("a.rs", 1)]);
        let shut: HashMap<String, bool> = [("a.rs".to_string(), false)].into_iter().collect();
        let rows = build_rows(&snap, &shut, None, DiffViewMode::Unified, false);
        assert_eq!(shape(&rows), ["file"]);
    }

    #[test]
    fn a_truncation_note_lands_under_the_lines_it_is_about() {
        let mut f = file("a.rs", 1);
        f.truncated = Some(Truncation::Budget);
        let rows = build_rows(
            &snapshot(vec![f]),
            &HashMap::new(),
            None,
            DiffViewMode::Unified,
            false,
        );
        assert_eq!(*shape(&rows).last().unwrap(), "truncated");
    }

    #[test]
    fn files_are_separated_by_a_gap_and_the_list_never_opens_with_one() {
        let snap = snapshot(vec![file("a.rs", 1), file("b.rs", 1)]);
        let shut: HashMap<String, bool> =
            snap.files.iter().map(|f| (f.path.clone(), false)).collect();
        let rows = build_rows(&snap, &shut, None, DiffViewMode::Unified, false);
        assert_eq!(shape(&rows), ["file", "gap", "file"]);
    }

    #[test]
    fn the_oversized_banner_leads_and_collapses_what_was_not_asked_for() {
        let snap = snapshot(vec![file("a.rs", 1), file("b.rs", 1)]);
        let rows = build_rows(&snap, &open(["b.rs"]), None, DiffViewMode::Unified, true);
        assert_eq!(
            shape(&rows),
            [
                "oversized",
                "gap",
                "file",
                "gap",
                "file",
                "hunk",
                "unified",
                "unified",
                "unified",
                "unified"
            ],
            "the file the reader opened stays open; the other one does not"
        );
    }

    #[test]
    fn a_focused_file_is_the_only_one_flattened() {
        let snap = snapshot(vec![file("a.rs", 1), file("b.rs", 1)]);
        let rows = build_rows(
            &snap,
            &HashMap::new(),
            Some(1),
            DiffViewMode::Unified,
            false,
        );
        assert_eq!(
            shape(&rows),
            ["file", "hunk", "unified", "unified", "unified", "unified"]
        );
        let DiffRow::FileHeader(head) = &rows[0] else {
            panic!("the focused file leads");
        };
        assert_eq!(head.path, "b.rs");
        assert_eq!(head.index, 1, "the element id still points into `files`");
    }

    #[test]
    fn the_file_list_is_capped_and_the_tail_gets_one_line() {
        let snap = snapshot(
            (0..MAX_RENDERED_FILES + 25)
                .map(|i| {
                    let mut f = file(&format!("f{i}.rs"), 1);
                    f.hunks.clear();
                    f
                })
                .collect(),
        );
        let rows = build_rows(&snap, &HashMap::new(), None, DiffViewMode::Unified, false);
        let files = rows
            .iter()
            .filter(|r| matches!(r, DiffRow::FileHeader(_)))
            .count();
        assert_eq!(files, MAX_RENDERED_FILES);
        assert!(matches!(rows.last(), Some(DiffRow::MoreFiles { rest: 25 })));
    }

    #[test]
    fn the_untracked_list_is_capped_but_its_count_stays_true() {
        let mut snap = snapshot(Vec::new());
        snap.untracked = (0..MAX_RENDERED_FILES + 5)
            .map(|i| format!("p{i}"))
            .collect();
        snap.untracked_total = 9_000;
        let rows = build_rows(&snap, &HashMap::new(), None, DiffViewMode::Unified, false);

        assert!(matches!(
            rows.first(),
            Some(DiffRow::UntrackedHeader { total: 9_000 })
        ));
        let shown = rows
            .iter()
            .filter(|r| matches!(r, DiffRow::Untracked { .. }))
            .count();
        assert_eq!(shown, MAX_RENDERED_FILES);
        assert!(matches!(
            rows.last(),
            Some(DiffRow::MoreUntracked { rest }) if *rest == 9_000 - MAX_RENDERED_FILES
        ));
    }

    #[test]
    fn a_preview_is_one_open_file() {
        let rows = preview_rows(&file("new.md", 1), DiffViewMode::Unified);
        assert_eq!(
            shape(&rows),
            ["file", "hunk", "unified", "unified", "unified", "unified"]
        );
        let DiffRow::FileHeader(head) = &rows[0] else {
            panic!("a file opens with its header");
        };
        assert_eq!(
            head.index,
            usize::MAX,
            "its element ids stay clear of the real list's"
        );
        assert!(head.expanded);
    }

    #[test]
    fn a_rename_shows_both_names_and_a_binary_file_cannot_be_opened() {
        let mut renamed = file("new.rs", 1);
        renamed.old_path = Some("old.rs".to_string());
        let rows = preview_rows(&renamed, DiffViewMode::Unified);
        let DiffRow::FileHeader(head) = &rows[0] else {
            unreachable!()
        };
        assert_eq!(head.shown_path, "old.rs → new.rs");
        assert_eq!(head.path, "new.rs", "the key is still the file itself");

        let mut binary = file("logo.png", 0);
        binary.binary = true;
        let rows = preview_rows(&binary, DiffViewMode::Unified);
        let DiffRow::FileHeader(head) = &rows[0] else {
            unreachable!()
        };
        assert!(!head.expandable);
    }

    /// The threshold that decides whether a file opens on its own, kept where
    /// the flattening that reads it lives.
    #[test]
    fn a_file_over_the_line_count_opens_only_when_asked() {
        let mut big = file("big.rs", 1);
        big.added = AUTO_COLLAPSE_LINES + 1;
        let none = HashMap::new();
        assert!(!file_expanded(&big, &none, false));
        assert!(file_expanded(&big, &open(["big.rs"]), true));
        assert!(file_expanded(&file("small.rs", 1), &none, false));
    }
}
