//! The changed-file list as a tree, and the filter both views share.
//!
//! Pure, and over plain paths rather than `StatusEntry`: the panel hands in
//! one group's paths and gets back rows that point into that same slice, so
//! the file rows it draws are the ones the flat list would have drawn — same
//! letter, same click, same context menu — only indented under a directory.
//!
//! Everything here comes out of the status the host already sent. A remote
//! repository gets the same tree without a single extra read of its disk.

use std::collections::BTreeMap;

/// One line of the tree view.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) enum TreeRow {
    /// A directory, possibly several compacted into one: `src/ui/scm` when
    /// `src` and `ui` hold nothing but the next directory down. `key` is the
    /// full repo-relative path, which is what the fold state is keyed by;
    /// `label` is what the row reads, relative to its parent row.
    Dir {
        key: String,
        label: String,
        depth: usize,
        /// Every file underneath, at any depth — what a folded row still
        /// owes the reader.
        files: usize,
        collapsed: bool,
    },
    /// A file, by its position in the paths the tree was built from.
    File { index: usize, depth: usize },
}

#[derive(Default)]
struct Node<'a> {
    dirs: BTreeMap<&'a str, Node<'a>>,
    files: Vec<(&'a str, usize)>,
}

impl Node<'_> {
    fn file_count(&self) -> usize {
        self.files.len() + self.dirs.values().map(Node::file_count).sum::<usize>()
    }
}

/// Group `paths` by directory, directories before files at every level and
/// each run in byte order — the order git itself lists paths in.
///
/// A directory whose only content is one more directory is folded into it,
/// the way VS Code's compact folders do: an agent's change three levels down
/// `crates/tty7-core/src` should cost one row to reach, not three.
///
/// `collapsed` is asked once per directory row with its full path. A folded
/// directory still gets its own row; only what is under it is left out.
pub(crate) fn tree_rows<'a>(
    paths: impl IntoIterator<Item = &'a str>,
    collapsed: impl Fn(&str) -> bool,
) -> Vec<TreeRow> {
    let mut root = Node::default();
    for (index, path) in paths.into_iter().enumerate() {
        // An untracked directory arrives as `dir/`. It is a leaf here, the
        // same as it is a single row in the flat list.
        let trimmed = path.strip_suffix('/').unwrap_or(path);
        let (dir, name) = match trimmed.rsplit_once('/') {
            Some((dir, name)) => (Some(dir), name),
            None => (None, trimmed),
        };
        let mut node = &mut root;
        for part in dir.into_iter().flat_map(|d| d.split('/')) {
            node = node.dirs.entry(part).or_default();
        }
        node.files.push((name, index));
    }
    let mut rows = Vec::new();
    emit(&root, "", 0, &collapsed, &mut rows);
    rows
}

fn emit(
    node: &Node<'_>,
    prefix: &str,
    depth: usize,
    collapsed: &impl Fn(&str) -> bool,
    rows: &mut Vec<TreeRow>,
) {
    for (name, child) in &node.dirs {
        let mut label = (*name).to_string();
        let mut child = child;
        while child.files.is_empty() && child.dirs.len() == 1 {
            let (next, grandchild) = child.dirs.iter().next().expect("one entry");
            label.push('/');
            label.push_str(next);
            child = grandchild;
        }
        let key = match prefix {
            "" => label.clone(),
            _ => format!("{prefix}/{label}"),
        };
        let folded = collapsed(&key);
        rows.push(TreeRow::Dir {
            key: key.clone(),
            label,
            depth,
            files: child.file_count(),
            collapsed: folded,
        });
        if !folded {
            emit(child, &key, depth + 1, collapsed, rows);
        }
    }
    let mut files = node.files.clone();
    files.sort_by(|a, b| a.0.cmp(b.0));
    rows.extend(
        files
            .into_iter()
            .map(|(_, index)| TreeRow::File { index, depth }),
    );
}

/// The filter box's text, normalised once: lowercased, trimmed, and `None`
/// when there is nothing left to filter by.
pub(crate) fn normalize_query(text: &str) -> Option<String> {
    let q = text.trim().to_lowercase();
    (!q.is_empty()).then_some(q)
}

/// Whether a changed path survives the filter.
///
/// Every whitespace-separated word has to appear somewhere in the path, in
/// any order: `scm panel` finds `src/ui/scm/panel.rs`, and so does `panel
/// scm`. `query` is what [`normalize_query`] returned.
pub(crate) fn path_matches(path: &str, query: &str) -> bool {
    let hay = path.to_lowercase();
    query.split_whitespace().all(|word| hay.contains(word))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open(paths: &[&str]) -> Vec<TreeRow> {
        tree_rows(paths.iter().copied(), |_| false)
    }

    fn dir(key: &str, label: &str, depth: usize, files: usize) -> TreeRow {
        TreeRow::Dir {
            key: key.into(),
            label: label.into(),
            depth,
            files,
            collapsed: false,
        }
    }

    fn file(index: usize, depth: usize) -> TreeRow {
        TreeRow::File { index, depth }
    }

    #[test]
    fn files_are_grouped_under_their_directories() {
        let rows = open(&["src/a.rs", "README.md", "src/b.rs", "docs/x.md"]);
        assert_eq!(
            rows,
            vec![
                dir("docs", "docs", 0, 1),
                file(3, 1),
                dir("src", "src", 0, 2),
                file(0, 1),
                file(2, 1),
                // Files after directories, at the root as everywhere else.
                file(1, 0),
            ]
        );
    }

    #[test]
    fn a_directory_holding_only_a_directory_is_compacted() {
        let rows = open(&[
            "crates/core/src/git/status.rs",
            "crates/core/src/git/diff.rs",
            "crates/core/Cargo.toml",
        ]);
        assert_eq!(
            rows,
            vec![
                // `crates` holds only `core`, so they share a row; `core`
                // holds a file, so the chain stops there.
                dir("crates/core", "crates/core", 0, 3),
                dir("crates/core/src/git", "src/git", 1, 2),
                file(1, 2),
                file(0, 2),
                file(2, 1),
            ]
        );
    }

    #[test]
    fn a_directory_with_two_children_is_not_compacted() {
        let rows = open(&["a/b/x.rs", "a/c/y.rs"]);
        assert_eq!(
            rows,
            vec![
                dir("a", "a", 0, 2),
                dir("a/b", "b", 1, 1),
                file(0, 2),
                dir("a/c", "c", 1, 1),
                file(1, 2),
            ]
        );
    }

    #[test]
    fn a_folded_directory_keeps_its_row_and_its_count() {
        let rows = tree_rows(["src/ui/a.rs", "src/ui/b.rs", "src/main.rs"], |key| {
            key == "src/ui"
        });
        assert_eq!(
            rows,
            vec![
                dir("src", "src", 0, 3),
                TreeRow::Dir {
                    key: "src/ui".into(),
                    label: "ui".into(),
                    depth: 1,
                    files: 2,
                    collapsed: true,
                },
                file(2, 1),
            ]
        );
    }

    #[test]
    fn folding_is_asked_about_the_compacted_path() {
        let asked = std::cell::RefCell::new(Vec::new());
        tree_rows(["a/b/c/x.rs"], |key| {
            asked.borrow_mut().push(key.to_string());
            false
        });
        assert_eq!(asked.into_inner(), vec!["a/b/c".to_string()]);
    }

    #[test]
    fn an_untracked_directory_is_a_leaf() {
        let rows = open(&["build/", "src/new/"]);
        assert_eq!(rows, vec![dir("src", "src", 0, 1), file(1, 1), file(0, 0)]);
    }

    #[test]
    fn every_path_lands_in_exactly_one_file_row() {
        let paths = [
            "z.rs",
            "a/b/c.rs",
            "a/b/d.rs",
            "a/e.rs",
            "f/g/h/i.rs",
            "f/j.rs",
            "a/b/",
        ];
        let mut seen: Vec<usize> = open(&paths)
            .into_iter()
            .filter_map(|row| match row {
                TreeRow::File { index, .. } => Some(index),
                TreeRow::Dir { .. } => None,
            })
            .collect();
        seen.sort_unstable();
        assert_eq!(seen, (0..paths.len()).collect::<Vec<_>>());
    }

    #[test]
    fn an_empty_list_is_an_empty_tree() {
        assert!(open(&[]).is_empty());
    }

    #[test]
    fn the_filter_needs_every_word_in_any_order_and_any_case() {
        let q = normalize_query("  Scm PANEL ").unwrap();
        assert!(path_matches("src/ui/scm/panel.rs", &q));
        assert!(path_matches("src/ui/Panel/SCM.rs", &q));
        assert!(!path_matches("src/ui/scm/state.rs", &q));
        let q = normalize_query("panel scm").unwrap();
        assert!(path_matches("src/ui/scm/panel.rs", &q));
    }

    #[test]
    fn a_blank_filter_is_no_filter() {
        assert_eq!(normalize_query(""), None);
        assert_eq!(normalize_query("   "), None);
        assert_eq!(normalize_query(" a "), Some("a".into()));
    }

    #[test]
    fn the_filter_reads_non_ascii_paths() {
        let q = normalize_query("设计").unwrap();
        assert!(path_matches("文档/设计/方案.md", &q));
        assert!(!path_matches("文档/实现.md", &q));
    }
}
