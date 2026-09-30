//! The Files tab: open a file in the editor by typing part of its name, the
//! way ⌘P does in VS Code, instead of walking the tree to it.
//!
//! The list comes from one walk of the project behind the active tab's panes,
//! done on the host that owns them through [`Host::search`]
//! — the same breadth-first, `.gitignore`-aware walk the Files panel's filter
//! runs, asked for every name at once. It is kept between openings and shown
//! at once when the search opens again, while a fresh walk runs behind it
//! (stale-while-revalidate): the list you already had is a better answer
//! than a spinner, and the one that replaces it lands under the cursor
//! without moving the highlighted row.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::{App, Context, Window};

use super::SearchTab;
use super::command::{CommandKind, Item};
use super::score::fuzzy_score;
use super::sources::{Row, Section, Source};
use crate::ui::app::Tty7App;
use crate::ui::host_ops::{Host, HostId, HostOps};
use crate::ui::i18n::{L10nKey, t_fmt};

/// How many entries one walk may return, directories included. A project
/// past this is mostly generated or vendored files, and a list the size of
/// the cap is already more than anyone scrolls; the walk is breadth-first, so
/// what the cap cuts off is the deepest part of the tree.
pub(crate) const INDEX_LIMIT: usize = 50_000;

/// How many directories one walk may open. Bounds a walk over a slow link as
/// much as it bounds the list: each directory is a round trip on a remote host.
pub(crate) const INDEX_MAX_DIRS: usize = 20_000;

/// A search that opens again this soon after a walk landed reuses it rather
/// than walking again. Opening and closing the search in quick succession is
/// someone looking for the right tab, not a sign the tree changed.
const REVALIDATE_AFTER: Duration = Duration::from_secs(5);

/// Rows the Files tab shows before anything is typed. A sample, shallowest
/// first, so the tab is visibly populated; the query is what finds things.
const BROWSE_ROWS: usize = 100;

/// Rows a query may bring back. Past this the ranking is noise, and every row
/// is an `Item` built on each keystroke.
const SEARCH_ROWS: usize = 200;

/// What a match only on the directory part of a path gives up against one on
/// the file's own name. Typing `app` means `app.rs` far more often than
/// `apps/web/index.ts`, but a query with a `/` in it can only match the path,
/// and those still have to come back.
const PATH_ONLY_PENALTY: i32 = 15;

/// One file the walk found.
#[derive(Debug)]
pub(crate) struct IndexedFile {
    /// Where it is on the host, as the host spells it.
    pub path: PathBuf,
    /// Where it is under its project root — the text the query matches and
    /// the row shows. With more than one root it starts with the root's name,
    /// so two `src/main.rs` in two projects can be told apart.
    pub rel: String,
    /// Where the file's own name starts in `rel`.
    pub name_at: usize,
    /// `rel` lowercased once, for the prefilter that runs on every keystroke.
    lower: String,
}

impl IndexedFile {
    pub(crate) fn name(&self) -> &str {
        &self.rel[self.name_at..]
    }

    /// The directory part of `rel`, without its trailing separator. Empty for
    /// a file at the root.
    pub(crate) fn dir(&self) -> &str {
        self.rel[..self.name_at].trim_end_matches(['/', '\\'])
    }
}

#[derive(Debug, Default)]
pub(crate) struct FileIndex {
    /// In the order the walk found them: shallowest first.
    pub files: Vec<IndexedFile>,
    /// Whether the walk stopped at [`INDEX_LIMIT`]. One cut short by
    /// [`INDEX_MAX_DIRS`] cannot tell, and is not flagged.
    pub capped: bool,
}

/// What the Files tab has to offer right now.
#[derive(Clone, Debug, Default)]
pub(crate) enum FileList {
    /// No pane has a directory worth searching: none reported one, or the one
    /// it reported is the home directory, which is not a project and whose
    /// walk would be all caches and downloads.
    #[default]
    NoRoots,
    /// The first walk of these folders has not come back yet.
    Indexing,
    /// The host could not walk them.
    Failed,
    Ready(Arc<FileIndex>),
}

/// A query as the Files tab reads it: the name to look for, and the place in
/// the file to land on when `:line` or `:line:column` follows it — what a
/// compiler prints and what people paste.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct FileQuery<'a> {
    pub needle: &'a str,
    pub line: Option<u32>,
    pub column: Option<u32>,
}

pub(crate) fn parse_query(query: &str) -> FileQuery<'_> {
    let query = query.trim();
    // A trailing colon is a line number on its way; it is not part of a name.
    let mut rest = query.strip_suffix(':').unwrap_or(query);
    let mut numbers: Vec<u32> = Vec::new();
    while numbers.len() < 2 {
        let Some((head, tail)) = rest.rsplit_once(':') else {
            break;
        };
        if head.is_empty() || tail.is_empty() || !tail.bytes().all(|b| b.is_ascii_digit()) {
            break;
        }
        let Ok(n) = tail.parse::<u32>() else { break };
        numbers.push(n);
        rest = head;
    }
    // Read right to left, so the last number found is the line.
    numbers.reverse();
    let nonzero = |n: Option<&u32>| n.copied().filter(|n| *n > 0);
    FileQuery {
        needle: rest,
        line: nonzero(numbers.first()),
        column: nonzero(numbers.get(1)),
    }
}

/// The folders to walk, from the ones the panes resolved to.
///
/// Home and a filesystem root are left out: neither is a project, and a walk
/// of either spends the whole budget on caches before it reaches anything
/// anyone meant to open. A folder inside another one on the list is left out
/// too — the outer walk already covers it, and listing it twice would put
/// every file in the results twice.
pub(crate) fn project_roots(resolved: Vec<PathBuf>, home: Option<&Path>) -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();
    for root in resolved {
        let is_home = home.is_some_and(|home| root == home);
        let is_fs_root = root.parent().is_none();
        if is_home || is_fs_root || roots.contains(&root) {
            continue;
        }
        roots.push(root);
    }
    let all = roots.clone();
    roots.retain(|r| !all.iter().any(|other| other != r && r.starts_with(other)));
    roots
}

/// The index for `files` found under `roots`.
pub(crate) fn build_index(roots: &[PathBuf], files: Vec<PathBuf>, capped: bool) -> FileIndex {
    let many = roots.len() > 1;
    let files = files
        .into_iter()
        .filter_map(|path| {
            let text = path.to_string_lossy().into_owned();
            // By text rather than `Path::strip_prefix`: a remote host's paths
            // may be Windows paths this client parses as one component.
            let (root, under) = roots
                .iter()
                .filter_map(|root| {
                    let root = root.to_string_lossy();
                    let under = text.strip_prefix(root.as_ref())?;
                    let under = under.trim_start_matches(['/', '\\']);
                    // `/repo-old/x` is not under `/repo`.
                    (under.len() < text.len() - root.len() || root.ends_with(['/', '\\']))
                        .then(|| (root.into_owned(), under.to_string()))
                })
                .max_by_key(|(root, _)| root.len())?;
            if under.is_empty() {
                return None;
            }
            let rel = match many {
                true => {
                    let name = root
                        .trim_end_matches(['/', '\\'])
                        .rsplit(['/', '\\'])
                        .next()
                        .unwrap_or_default();
                    format!("{name}/{under}")
                }
                false => under,
            };
            let name_at = rel.rfind(['/', '\\']).map_or(0, |i| i + 1);
            let lower = rel.to_lowercase();
            Some(IndexedFile {
                path,
                rel,
                name_at,
                lower,
            })
        })
        .collect();
    FileIndex { files, capped }
}

/// Whether every character of `needle` (already lowercase, no whitespace)
/// appears in `hay` in order. Cheap enough to run over the whole index on
/// every keystroke, and it throws out nearly everything before the real
/// scorer, which allocates, sees it.
fn is_subsequence(needle: &[char], hay: &str) -> bool {
    let mut want = needle.iter().peekable();
    for c in hay.chars() {
        match want.peek() {
            Some(w) if **w == c => {
                want.next();
            }
            Some(_) => {}
            None => break,
        }
    }
    want.peek().is_none()
}

/// How well `file` answers `needle`, on the one scale every tab shares — so
/// the All tab can put a file beside an action and have the order mean
/// something. A hit on the file's own name wins over the same hit spread
/// across its directories.
pub(crate) fn file_score(needle: &str, file: &IndexedFile) -> Option<i32> {
    let on_name = fuzzy_score(needle, file.name());
    let on_path = fuzzy_score(needle, &file.rel).map(|s| s - PATH_ONLY_PENALTY);
    on_name.into_iter().chain(on_path).max()
}

/// The files that match `needle`, best first, at most `limit` of them. Ties
/// go to the shorter path — the shallower file is the likelier one.
pub(crate) fn rank<'a>(
    index: &'a FileIndex,
    needle: &str,
    limit: usize,
) -> Vec<(i32, &'a IndexedFile)> {
    let chars: Vec<char> = needle
        .chars()
        .flat_map(char::to_lowercase)
        .filter(|c| !c.is_whitespace())
        .collect();
    if chars.is_empty() {
        return Vec::new();
    }
    let mut hits: Vec<(i32, &IndexedFile)> = index
        .files
        .iter()
        .filter(|f| is_subsequence(&chars, &f.lower))
        .filter_map(|f| Some((file_score(needle, f)?, f)))
        .collect();
    hits.sort_by(|(a, fa), (b, fb)| b.cmp(a).then_with(|| fa.rel.len().cmp(&fb.rel.len())));
    hits.truncate(limit);
    hits
}

fn file_item(file: &IndexedFile, line: Option<u32>, column: Option<u32>) -> Item {
    let mut item = Item::new(
        file.name(),
        CommandKind::OpenFile {
            path: file.path.clone(),
            line,
            column,
        },
    );
    if !file.dir().is_empty() {
        item = item.with_subtitle(file.dir());
    }
    if let Some(line) = line {
        item = item.with_note(t_fmt(
            L10nKey::SearchFilesGoToLine,
            &[("line", &line.to_string())],
        ));
    }
    item
}

pub(super) struct Files<'a>(pub &'a FileList);

impl Source for Files<'_> {
    fn tab(&self) -> SearchTab {
        SearchTab::Files
    }

    fn browse(&self, _cx: &App) -> Vec<Section> {
        let FileList::Ready(index) = self.0 else {
            return Vec::new();
        };
        let rows: Vec<Row> = index
            .files
            .iter()
            .take(BROWSE_ROWS)
            .map(|f| Row::Item(file_item(f, None, None)))
            .collect();
        if rows.is_empty() {
            return Vec::new();
        }
        // A walk that hit its cap left the deepest files out, and a search
        // that cannot find one of them should not look like a missing file.
        let title = index.capped.then(|| {
            t_fmt(
                L10nKey::SearchFilesCapped,
                &[("count", &index.files.len().to_string())],
            )
            .into()
        });
        vec![Section { title, rows }]
    }

    fn highlights(&self, _cx: &App) -> Vec<Item> {
        Vec::new()
    }

    /// A sample of the project is not an answer to anything, and on the All
    /// tab it would push the tabs you were just in down the page.
    fn on_the_empty_all_tab(&self) -> bool {
        false
    }

    fn search(&self, query: &str, _cx: &App) -> Vec<(i32, Item)> {
        let FileList::Ready(index) = self.0 else {
            return Vec::new();
        };
        let q = parse_query(query);
        rank(index, q.needle, SEARCH_ROWS)
            .into_iter()
            .map(|(score, f)| (score, file_item(f, q.line, q.column)))
            .collect()
    }
}

/// Which folders a walk is of: the host, and the directories the active tab's
/// panes are in. The walk resolves them to their repositories itself, on the
/// host, so the key is what the UI thread can know without asking.
#[derive(Clone, Debug, PartialEq, Eq)]
struct IndexKey {
    host: HostId,
    cwds: Vec<PathBuf>,
}

struct Walked {
    key: IndexKey,
    list: FileList,
    at: Instant,
}

/// The window's file index, kept between openings of the search.
#[derive(Default)]
pub(crate) struct FileIndexStore {
    last: Option<Walked>,
    /// The walk in flight, if any: its generation and what it is walking.
    /// A walk that lands after a newer one started is dropped.
    walking: Option<(u64, IndexKey)>,
    generation: u64,
}

/// Walks the project the panes in `cwds` are in, on the host that has it.
/// Runs off the UI thread.
pub(crate) fn walk(h: &dyn Host, cwds: &[PathBuf], home: Option<&Path>) -> FileList {
    let resolved = cwds
        .iter()
        .map(|cwd| {
            h.repo_root(cwd)
                .ok()
                .flatten()
                .unwrap_or_else(|| cwd.clone())
        })
        .collect();
    let roots = project_roots(resolved, home);
    if roots.is_empty() {
        return FileList::NoRoots;
    }
    // Hidden and gitignored entries stay out, as they do in the tree by
    // default: that is what keeps `.git`, `target/` and `node_modules/` from
    // filling the cap before the sources. The empty query matches every name.
    match h.search(&roots, "", INDEX_LIMIT, INDEX_MAX_DIRS, false) {
        Ok(hits) => {
            let capped = hits.len() >= INDEX_LIMIT;
            let files = hits
                .into_iter()
                .filter(|hit| !hit.is_dir)
                .map(|hit| hit.path)
                .collect();
            FileList::Ready(Arc::new(build_index(&roots, files, capped)))
        }
        Err(e) => {
            log::warn!("quick open: walking {roots:?}: {e}");
            FileList::Failed
        }
    }
}

impl Tty7App {
    /// Search Everywhere on its Files tab — or closed, when that is what it
    /// is already showing, so the chord that opens it also puts it away.
    pub(crate) fn quick_open_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(view) = self.search.clone() {
            if view.read(cx).tab() == SearchTab::Files {
                self.close_search(window, cx);
            } else {
                view.update(cx, |view, cx| {
                    view.set_tab(SearchTab::Files, None, window, cx)
                });
            }
            return;
        }
        self.open_search(SearchTab::Files, "", window, cx);
    }

    /// Opens a row of the Files tab, and has the tree follow along the way a
    /// file link in the grid does.
    pub(crate) fn open_indexed_file(
        &mut self,
        path: &Path,
        line: Option<u32>,
        column: Option<u32>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_file_in_editor_at(path, line, column, window, cx);
        self.file_tree_reveal_path(path, cx);
    }

    fn file_index_key(&self, cx: &App) -> IndexKey {
        let host = self.spawn_host(cx);
        let mut cwds: Vec<PathBuf> = self
            .tabs
            .get(self.active)
            .map(|tab| tab.pane.terminals())
            .unwrap_or_default()
            .iter()
            .filter(|leaf| leaf.read(cx).host_id() == host)
            // `files_cwd`, for the reason the Files panel roots itself there:
            // it follows an agent into its worktree, and it spells a WSL
            // pane's directory the way the host can read it.
            .filter_map(|leaf| leaf.read(cx).files_cwd())
            .collect();
        cwds.sort();
        cwds.dedup();
        IndexKey { host, cwds }
    }

    /// What the Files tab opens on: the last walk, if it was of the folders
    /// the panes are in now.
    pub(crate) fn file_list_now(&self, cx: &App) -> FileList {
        let key = self.file_index_key(cx);
        if key.cwds.is_empty() {
            return FileList::NoRoots;
        }
        match &self.file_tree.quick_open.last {
            Some(last) if last.key == key => last.list.clone(),
            _ => FileList::Indexing,
        }
    }

    /// Walks the project again, unless a walk of it just landed or is
    /// already on its way, and hands the result to the open search.
    pub(crate) fn refresh_file_index(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let key = self.file_index_key(cx);
        if key.cwds.is_empty() {
            return;
        }
        let store = &self.file_tree.quick_open;
        if store.walking.as_ref().is_some_and(|(_, k)| *k == key) {
            return;
        }
        if store
            .last
            .as_ref()
            .is_some_and(|l| l.key == key && l.at.elapsed() < REVALIDATE_AFTER)
        {
            return;
        }
        let Some(host) = self.active_host(cx) else {
            return;
        };
        // Only this computer's home is known here, and it is the common case:
        // a fresh local tab starts there.
        let home = key
            .host
            .is_local()
            .then(|| std::env::var_os("HOME").map(PathBuf::from))
            .flatten();
        let store = &mut self.file_tree.quick_open;
        store.generation += 1;
        let generation = store.generation;
        store.walking = Some((generation, key.clone()));
        let cwds = key.cwds.clone();
        HostOps::run_in(
            host,
            window,
            cx,
            move |h| walk(h, &cwds, home.as_deref()),
            move |app, list, window, cx| {
                let store = &mut app.file_tree.quick_open;
                if store.walking.as_ref().map(|(g, _)| *g) != Some(generation) {
                    return;
                }
                store.walking = None;
                // A revalidation that failed keeps the list it was checking:
                // yesterday's files are still mostly where they were.
                let keep_old = matches!(list, FileList::Failed)
                    && store
                        .last
                        .as_ref()
                        .is_some_and(|l| l.key == key && matches!(l.list, FileList::Ready(_)));
                if !keep_old {
                    store.last = Some(Walked {
                        key: key.clone(),
                        list,
                        at: Instant::now(),
                    });
                }
                if app.file_index_key(cx) != key {
                    return;
                }
                let list = app.file_list_now(cx);
                if let Some(view) = app.search.clone() {
                    view.update(cx, |view, cx| view.set_files(list, window, cx));
                }
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn index(paths: &[&str]) -> FileIndex {
        build_index(
            &[PathBuf::from("/repo")],
            paths
                .iter()
                .map(|p| PathBuf::from(format!("/repo/{p}")))
                .collect(),
            false,
        )
    }

    fn ranked(index: &FileIndex, query: &str) -> Vec<String> {
        rank(index, parse_query(query).needle, 50)
            .into_iter()
            .map(|(_, f)| f.rel.clone())
            .collect()
    }

    #[test]
    fn a_query_can_name_a_line_and_a_column() {
        let q = |s| parse_query(s);
        assert_eq!(
            q("main.rs"),
            FileQuery {
                needle: "main.rs",
                line: None,
                column: None
            }
        );
        assert_eq!(
            q("main.rs:120"),
            FileQuery {
                needle: "main.rs",
                line: Some(120),
                column: None
            }
        );
        assert_eq!(
            q("src/main.rs:120:7"),
            FileQuery {
                needle: "src/main.rs",
                line: Some(120),
                column: Some(7)
            }
        );
        // Half typed: the colon is on its way to a number.
        assert_eq!(q("main.rs:").needle, "main.rs");
        assert_eq!(q("main.rs:12:").line, Some(12));
        // Not a number, so part of the name.
        assert_eq!(q("std::fs").needle, "std::fs");
        assert_eq!(q("a:b12").needle, "a:b12");
        // A bare number is a name to look for, not a line in nothing.
        assert_eq!(q("404").needle, "404");
        assert_eq!(q(":12").needle, ":12");
        // Line 0 does not exist.
        assert_eq!(q("x.rs:0").line, None);
        assert_eq!(q("  x.rs:3  ").line, Some(3));
    }

    #[test]
    fn a_match_on_the_name_beats_the_same_letters_in_the_path() {
        let idx = index(&["apps/web/index.ts", "src/app.rs", "src/ui/app.rs"]);
        let got = ranked(&idx, "app");
        assert_eq!(
            &got[..2],
            ["src/app.rs", "src/ui/app.rs"],
            "the shorter of two equal name hits leads"
        );
        assert_eq!(got[2], "apps/web/index.ts");
    }

    #[test]
    fn an_exact_name_outranks_a_longer_one() {
        let idx = index(&["src/ui/main_window.rs", "src/main.rs"]);
        assert_eq!(ranked(&idx, "main.rs")[0], "src/main.rs");
    }

    #[test]
    fn a_path_query_finds_files_by_their_directories() {
        let idx = index(&["src/ui/search/view.rs", "src/ui/view.rs", "docs/view.md"]);
        let got = ranked(&idx, "search/view");
        assert_eq!(got, ["src/ui/search/view.rs"]);
        let got = ranked(&idx, "ui view");
        assert_eq!(got.len(), 2, "whitespace is ignored, as everywhere else");
    }

    #[test]
    fn nothing_that_is_not_a_subsequence_matches() {
        let idx = index(&["src/main.rs"]);
        assert!(ranked(&idx, "zzz").is_empty());
        assert!(ranked(&idx, "").is_empty(), "an empty query ranks nothing");
    }

    #[test]
    fn rows_split_into_a_name_and_the_directory_it_is_in() {
        let idx = index(&["src/ui/app.rs", "README.md"]);
        assert_eq!(idx.files[0].name(), "app.rs");
        assert_eq!(idx.files[0].dir(), "src/ui");
        assert_eq!(idx.files[1].name(), "README.md");
        assert_eq!(idx.files[1].dir(), "");
    }

    #[test]
    fn two_roots_are_told_apart_by_name() {
        let idx = build_index(
            &[PathBuf::from("/work/api"), PathBuf::from("/work/web")],
            vec![
                PathBuf::from("/work/api/src/main.rs"),
                PathBuf::from("/work/web/src/main.rs"),
                // A sibling whose name merely starts with a root's is not in it.
                PathBuf::from("/work/api-old/x.rs"),
            ],
            false,
        );
        let rels: Vec<_> = idx.files.iter().map(|f| f.rel.as_str()).collect();
        assert_eq!(rels, ["api/src/main.rs", "web/src/main.rs"]);
    }

    #[test]
    fn a_windows_path_splits_on_its_own_separator() {
        let idx = build_index(
            &[PathBuf::from(r"C:\repo")],
            vec![PathBuf::from(r"C:\repo\src\lib.rs")],
            false,
        );
        assert_eq!(idx.files[0].rel, r"src\lib.rs");
        assert_eq!(idx.files[0].name(), "lib.rs");
        assert_eq!(idx.files[0].dir(), "src");
    }

    #[test]
    fn home_and_the_filesystem_root_are_not_projects() {
        let home = PathBuf::from("/home/me");
        let roots = project_roots(
            vec![
                home.clone(),
                PathBuf::from("/"),
                PathBuf::from("/home/me/repo"),
                PathBuf::from("/home/me/repo"),
                PathBuf::from("/home/me/repo/crates/core"),
                PathBuf::from("/srv/other"),
            ],
            Some(&home),
        );
        assert_eq!(
            roots,
            [PathBuf::from("/home/me/repo"), PathBuf::from("/srv/other")],
            "home and / go, a duplicate goes, and a folder inside another is covered by it"
        );
        assert!(project_roots(vec![home.clone()], Some(&home)).is_empty());
    }

    /// The walk goes through the host, from the pane's directory up to its
    /// repository, and leaves out what the tree hides by default.
    #[test]
    fn the_walk_indexes_the_repository_without_what_git_ignores() {
        let host = tty7_core::host::local::LocalHost::new();
        let tmp = std::env::temp_dir().join(format!("tty7-quick-open-{}", std::process::id()));
        let _ = host.remove(&tmp, true);
        host.create_dir(&tmp.join(".git"), true).unwrap();
        host.create_dir(&tmp.join("src/ui"), true).unwrap();
        host.create_dir(&tmp.join("target/debug"), true).unwrap();
        host.write_file(&tmp.join(".gitignore"), b"target/\n")
            .unwrap();
        host.write_file(&tmp.join(".git/HEAD"), b"").unwrap();
        host.write_file(&tmp.join("target/debug/out.rs"), b"")
            .unwrap();
        host.write_file(&tmp.join("README.md"), b"").unwrap();
        host.write_file(&tmp.join("src/ui/app.rs"), b"").unwrap();

        // From a subdirectory: the walk is of the whole repository.
        let FileList::Ready(idx) = walk(&*host, &[tmp.join("src")], None) else {
            panic!("the walk failed");
        };
        let mut rels: Vec<_> = idx.files.iter().map(|f| f.rel.replace('\\', "/")).collect();
        rels.sort();
        assert_eq!(rels, ["README.md", "src/ui/app.rs"]);
        assert!(!idx.capped);

        assert!(matches!(
            walk(&*host, std::slice::from_ref(&tmp), Some(&tmp)),
            FileList::NoRoots
        ));
        let _ = host.remove(&tmp, true);
    }

    #[test]
    fn a_long_index_is_ranked_quickly_enough_to_type_into() {
        let paths: Vec<String> = (0..INDEX_LIMIT)
            .map(|i| format!("crates/c{}/src/module_{i}/file_{i}.rs", i % 97))
            .collect();
        let refs: Vec<&str> = paths.iter().map(String::as_str).collect();
        let idx = index(&refs);
        let started = Instant::now();
        let got = rank(&idx, "file_4242", 20);
        assert_eq!(got[0].1.name(), "file_4242.rs");
        // Generous, so a loaded CI box does not flake; the point is that a
        // full index is not seconds of work per keystroke.
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "ranking {INDEX_LIMIT} files took {:?}",
            started.elapsed()
        );
    }
}
