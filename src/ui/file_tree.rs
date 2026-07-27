//! Local project file tree (the code overlay's left column — see
//! `code_editor::render_code_overlay` for the panel that hosts it).
//!
//! Modelled on Warp's Project Explorer: lazily-loaded directories, gitignore
//! awareness (ignored entries render dimmed, not hidden), a filesystem watcher
//! that keeps listings fresh, keyboard navigation, inline new-file / rename
//! editing, and a per-row context menu (open / cd / reveal / copy path /
//! delete / attach to a coding agent). Roots come from the active tab's panes:
//! each pane's cwd resolves to its repository root (walk up to `.git`), so a
//! tab whose panes sit in two repos shows both as top-level roots.
//!
//! The panel is a plain lazy tree over `std::fs` — no daemon round-trips (the
//! SFTP panel covers the remote case). Listings are cached per directory and
//! invalidated by `notify` events, so a huge repo only ever pays for the
//! directories actually expanded.
//!
//! Every `read_dir` runs on the background executor: render only ever reads the
//! cache, and a miss turns into a queued load whose answer lands with a
//! `cx.notify()` a frame or more later. A directory the user just expanded is
//! therefore empty for one paint, which is what the cache miss costs and what
//! every other editor does — far better than stalling the frame on a cold
//! `.gitignore` chain or, for the search box, on a 2000-directory walk.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use gpui::prelude::*;
use gpui::{
    AnyElement, Context, Entity, ExternalPaths, FocusHandle, KeyDownEvent, MouseButton,
    PromptLevel, SharedString, Subscription, Window, div, px,
};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::menu::{ContextMenuExt as _, PopupMenu, PopupMenuItem};
use gpui_component::{ActiveTheme as _, Icon, IconName, Sizable as _, h_flex, v_flex};
use ignore::gitignore::Gitignore;

use crate::ui::app::Tty7App;

/// Per-level indent (px) for nested rows.
const INDENT: f32 = 14.0;

/// Debounce for watcher-driven refreshes (same rationale as the config
/// hot-reload: coalesce a save burst into one reload).
const REFRESH_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(200);

/// Debounce for the Files-tab search box. Each query walks up to `MAX_DIRS`
/// directories, so only the pause after the last keystroke should pay for a
/// walk — typing "src" otherwise buys three of them.
const SEARCH_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(200);

/// One directory entry in a cached listing.
#[derive(Clone)]
pub(crate) struct TreeEntry {
    pub name: String,
    pub path: PathBuf,
    pub is_dir: bool,
    /// Matched by the gitignore chain (or is `.git` itself): rendered dimmed.
    pub ignored: bool,
}

/// A flattened visible row: what the list renders and what keyboard
/// navigation walks. Roots are rows too (depth 0, always expanded-looking).
pub(crate) struct TreeRow {
    pub entry: TreeEntry,
    pub depth: usize,
    pub is_root: bool,
    pub expanded: bool,
}

/// One in-progress inline edit (new file / new folder / rename).
pub(crate) enum TreeEdit {
    NewFile {
        dir: PathBuf,
        input: Entity<InputState>,
    },
    NewFolder {
        dir: PathBuf,
        input: Entity<InputState>,
    },
    Rename {
        path: PathBuf,
        input: Entity<InputState>,
    },
}

impl TreeEdit {
    fn input(&self) -> &Entity<InputState> {
        match self {
            TreeEdit::NewFile { input, .. }
            | TreeEdit::NewFolder { input, .. }
            | TreeEdit::Rename { input, .. } => input,
        }
    }

    /// The directory whose listing hosts the edit row.
    fn host_dir(&self) -> &Path {
        match self {
            TreeEdit::NewFile { dir, .. } | TreeEdit::NewFolder { dir, .. } => dir,
            TreeEdit::Rename { path, .. } => path.parent().unwrap_or(path),
        }
    }
}

/// The filesystem half of the tree, moved onto the background executor so a
/// paint never blocks on `read_dir`. It owns everything a listing needs —
/// the `.gitignore` matchers included — because the UI thread's copies sit
/// behind `&mut FileTreeState`, which no background task can hold. Seeded from
/// [`FileTreeState::gitignore`] so most loads re-use matchers already compiled,
/// and handed back on landing so the ones it compiled itself are re-used next
/// time. `Arc`, not `Rc`, for exactly that trip across threads.
struct TreeLoader {
    gitignore: HashMap<PathBuf, Option<Arc<Gitignore>>>,
    show_hidden: bool,
}

impl TreeLoader {
    /// Read one directory into sorted entries, tagging gitignored ones.
    fn list_dir(&mut self, dir: &Path, root: &Path) -> Vec<TreeEntry> {
        let Ok(read) = std::fs::read_dir(dir) else {
            return Vec::new();
        };
        let mut out: Vec<TreeEntry> = Vec::new();
        for e in read.flatten() {
            let path = e.path();
            let name = e.file_name().to_string_lossy().to_string();
            let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
            let ignored = name == ".git" || self.is_gitignored(&path, is_dir, root);
            out.push(TreeEntry {
                name,
                path,
                is_dir,
                ignored,
            });
        }
        sort_entries(&mut out);
        out
    }

    /// Walk the `.gitignore` chain from `root` down to the entry's directory;
    /// the deepest match wins (whitelist `!patterns` un-ignore).
    fn is_gitignored(&mut self, path: &Path, is_dir: bool, root: &Path) -> bool {
        let Some(parent) = path.parent() else {
            return false;
        };
        let mut state = false;
        // Ancestor chain root → parent, in order.
        let mut chain: Vec<&Path> = parent
            .ancestors()
            .take_while(|a| a.starts_with(root))
            .collect();
        chain.reverse();
        for dir in chain {
            let gi = self
                .gitignore
                .entry(dir.to_path_buf())
                .or_insert_with(|| {
                    let file = dir.join(".gitignore");
                    file.is_file().then(|| {
                        let (gi, _err) = Gitignore::new(&file);
                        Arc::new(gi)
                    })
                })
                .clone();
            let Some(gi) = gi else { continue };
            let Ok(rel) = path.strip_prefix(dir) else {
                continue;
            };
            match gi.matched(rel, is_dir) {
                ignore::Match::Ignore(_) => state = true,
                ignore::Match::Whitelist(_) => state = false,
                ignore::Match::None => {}
            }
        }
        state
    }

    /// Flat, bounded search across the whole tree — not a filter over the rows
    /// that happen to be expanded, which would answer "no matches" for anything
    /// the user hasn't already drilled into. Walks breadth-first from the roots
    /// so shallow hits (the ones you usually mean) come first, skips ignored
    /// directories entirely — `.git`, `target`, `node_modules` are where the file
    /// count explodes and never where you're searching — and stops at `LIMIT`
    /// hits so a query like "e" can't walk a whole monorepo.
    ///
    /// Deliberately listing every directory itself rather than consulting the
    /// UI thread's cache: a breadth-first walk visits each directory once, so
    /// the cache would only ever save the handful the user has expanded, and
    /// sharing it would mean sending thousands of listings back to be stored.
    fn search(&mut self, roots: &[PathBuf], query: &str) -> Vec<TreeEntry> {
        const LIMIT: usize = 200;
        /// Directories visited even if nothing matches, so a typo can't turn into
        /// a full-disk crawl.
        const MAX_DIRS: usize = 2000;

        let needle = query.to_lowercase();
        let mut out: Vec<TreeEntry> = Vec::new();
        let mut visited = 0usize;
        for root in roots {
            // A deque, not a `Vec` + `remove(0)`: the breadth-first frontier of a
            // wide tree gets long, and shifting it down per pop is quadratic.
            let mut queue: std::collections::VecDeque<PathBuf> =
                std::collections::VecDeque::from([root.clone()]);
            while let Some(dir) = queue.pop_front() {
                if out.len() >= LIMIT || visited >= MAX_DIRS {
                    break;
                }
                visited += 1;
                for e in self.list_dir(&dir, root) {
                    if e.ignored && !self.show_hidden {
                        continue;
                    }
                    if !self.show_hidden && e.name.starts_with('.') {
                        continue;
                    }
                    if e.is_dir {
                        queue.push_back(e.path.clone());
                    }
                    if e.name.to_lowercase().contains(&needle) {
                        out.push(e);
                        if out.len() >= LIMIT {
                            break;
                        }
                    }
                }
            }
        }
        out
    }
}

/// Which directories have a background listing out, and which of those were
/// invalidated while it flew. Two jobs: render re-asks for the same missing
/// directory every frame until the answer lands, so `in_flight` keeps that from
/// spawning a load per frame; and a watcher event landing mid-load would
/// otherwise let the pre-change listing win the race, so `stale` makes the
/// answer drop itself and the next render ask again.
#[derive(Default)]
struct Loads {
    in_flight: HashSet<PathBuf>,
    stale: HashSet<PathBuf>,
}

impl Loads {
    /// `true` when the caller should spawn — nothing is out for `dir` yet.
    fn begin(&mut self, dir: &Path) -> bool {
        self.in_flight.insert(dir.to_path_buf())
    }

    /// Record that a filesystem change superseded whatever is in flight for
    /// `dir` (a no-op when nothing is).
    fn invalidate(&mut self, dir: &Path) {
        if self.in_flight.contains(dir) {
            self.stale.insert(dir.to_path_buf());
        }
    }

    /// Every in-flight answer is now stale — used when the whole cache goes.
    fn invalidate_all(&mut self) {
        self.stale.extend(self.in_flight.iter().cloned());
    }

    /// Retire the load for `dir`: `true` when its answer is still current and
    /// may be cached, `false` when it must be thrown away.
    fn finish(&mut self, dir: &Path) -> bool {
        self.in_flight.remove(dir);
        !self.stale.remove(dir)
    }
}

/// The Files-tab search box's off-thread state: the query the newest walk
/// covers, the generation that identifies it, and the hits last accepted.
#[derive(Default)]
struct SearchState {
    /// Bumped per walk so a slow one can't overwrite a newer one's answer —
    /// same guard as `right_panel`'s process poll.
    generation: u64,
    /// The query the in-flight (or last completed) walk covers. Render compares
    /// against the live input, so a repaint mid-walk doesn't queue a second one.
    pending: String,
    /// The dotfile setting that walk ran under. The walk bakes it in — hidden
    /// and ignored entries never enter the hits — so flipping the eye toggle
    /// has to re-walk, even though the query never moved.
    hidden: bool,
    hits: Vec<TreeEntry>,
}

impl SearchState {
    /// Point the search at `query`, returning the generation a fresh walk
    /// should carry — `None` when the current one already covers it. An empty
    /// query drops the hits so the next one can't flash the previous one's
    /// results before its own land.
    fn retarget(&mut self, query: &str, show_hidden: bool) -> Option<u64> {
        if self.pending == query && self.hidden == show_hidden {
            return None;
        }
        self.generation += 1;
        self.pending = query.to_string();
        self.hidden = show_hidden;
        if query.is_empty() {
            self.hits.clear();
            return None;
        }
        Some(self.generation)
    }

    /// Take a landed walk's hits unless a newer query superseded it.
    fn accept(&mut self, generation: u64, hits: Vec<TreeEntry>) -> bool {
        if self.generation != generation {
            return false;
        }
        self.hits = hits;
        true
    }

    /// Forget both the in-flight walk and what it covered, so the next render
    /// starts a new one for the same query. For when the ground moved under it
    /// (the ignore rules changed, or the tree got new roots) rather than the
    /// query changing.
    fn restart(&mut self) {
        self.generation += 1;
        self.pending.clear();
    }
}

/// App-global file-tree infrastructure, held on [`Tty7App`]. The per-tab view
/// state (roots, expansion, selection) lives in
/// [`TabCode`](crate::ui::code_editor::TabCode); everything here is path-keyed
/// cache or chrome shared by every tab's panel — one panel shows at a time.
pub(crate) struct FileTreeState {
    /// Lazily-loaded listing per directory; invalidated by watcher events. The
    /// only thing render reads — a miss is a queued load, never a `read_dir`.
    children: HashMap<PathBuf, Vec<TreeEntry>>,
    /// Compiled `.gitignore` per directory (`None` = the dir has none).
    /// Invalidated when a `.gitignore` changes.
    gitignore: HashMap<PathBuf, Option<Arc<Gitignore>>>,
    loads: Loads,
    search: SearchState,
    pub(crate) show_hidden: bool,
    pub(crate) editing: Option<TreeEdit>,
    editing_subs: Vec<Subscription>,
    /// One recursive watcher over the union of every tab's roots; rebuilt
    /// when any root set changes. Events invalidate the path-keyed caches
    /// above, which are tab-agnostic.
    watcher: Option<notify::RecommendedWatcher>,
    /// What `watcher` currently spans. The union can move without the active
    /// tab's roots moving — closing a tab drops roots from it — so the rebuild
    /// check compares this rather than trusting the per-tab comparison.
    watched: HashSet<PathBuf>,
    events_tx: smol::channel::Sender<PathBuf>,
    pub(crate) focus_handle: FocusHandle,
}

impl FileTreeState {
    pub(crate) fn new(window: &mut Window, cx: &mut Context<Tty7App>) -> Self {
        let (tx, rx) = smol::channel::unbounded::<PathBuf>();
        cx.spawn_in(window, async move |app, cx| {
            while let Ok(first) = rx.recv().await {
                cx.background_executor().timer(REFRESH_DEBOUNCE).await;
                let mut changed: HashSet<PathBuf> = HashSet::from([first]);
                while let Ok(more) = rx.try_recv() {
                    changed.insert(more);
                }
                let ok = app.update(cx, |app, cx| {
                    app.file_tree_apply_fs_events(&changed, cx);
                });
                if ok.is_err() {
                    break;
                }
            }
        })
        .detach();
        Self {
            children: HashMap::new(),
            gitignore: HashMap::new(),
            loads: Loads::default(),
            search: SearchState::default(),
            show_hidden: false,
            editing: None,
            editing_subs: Vec::new(),
            watcher: None,
            watched: HashSet::new(),
            events_tx: tx,
            focus_handle: cx.focus_handle(),
        }
    }

    /// (Re)attach the recursive watcher to `roots` (the union across tabs).
    fn rebuild_watcher(&mut self, roots: &HashSet<PathBuf>) {
        use notify::{RecursiveMode, Watcher};
        self.watcher = None;
        // Recorded before the watches go on, not after: a root that fails to
        // watch is logged once, not retried on every render.
        self.watched = roots.clone();
        if roots.is_empty() {
            return;
        }
        let tx = self.events_tx.clone();
        let handler = move |res: notify::Result<notify::Event>| {
            let Ok(event) = res else { return };
            for p in event.paths {
                let _ = tx.try_send(p);
            }
        };
        let mut watcher = match notify::recommended_watcher(handler) {
            Ok(w) => w,
            Err(e) => {
                log::warn!("file tree: watcher unavailable: {e}");
                return;
            }
        };
        for root in roots {
            if let Err(e) = watcher.watch(root, RecursiveMode::Recursive) {
                log::warn!("file tree: failed to watch {}: {e}", root.display());
            }
        }
        self.watcher = Some(watcher);
    }

    /// A background worker seeded with the matchers compiled so far.
    fn loader(&self) -> TreeLoader {
        TreeLoader {
            gitignore: self.gitignore.clone(),
            show_hidden: self.show_hidden,
        }
    }

    /// Ask for a listing of every root and expanded directory that isn't cached
    /// yet. Called from render, so it must stay map lookups: the `read_dir`
    /// runs on the background executor and the answer arrives with a
    /// `cx.notify()`, which is why a just-expanded directory fills in on the
    /// next frame rather than this one.
    fn request_loads(
        &mut self,
        roots: &[PathBuf],
        expanded: &HashSet<PathBuf>,
        cx: &mut Context<Tty7App>,
    ) {
        // Roots always list; expanded dirs list on demand.
        for root in roots {
            self.request_load(root.clone(), root.clone(), cx);
            for dir in expanded {
                if dir.starts_with(root) {
                    self.request_load(dir.clone(), root.clone(), cx);
                }
            }
        }
    }

    /// Spawn one directory listing, unless it's already cached or already out.
    fn request_load(&mut self, dir: PathBuf, root: PathBuf, cx: &mut Context<Tty7App>) {
        if self.children.contains_key(&dir) || !self.loads.begin(&dir) {
            return;
        }
        let mut loader = self.loader();
        cx.spawn(async move |app, cx| {
            let (entries, gitignore) = cx
                .background_executor()
                .spawn({
                    let dir = dir.clone();
                    async move {
                        let entries = loader.list_dir(&dir, &root);
                        (entries, loader.gitignore)
                    }
                })
                .await;
            let _ = app.update(cx, |app, cx| {
                if app.file_tree.loads.finish(&dir) {
                    // The matchers ride along with the listing they justified:
                    // dropping a stale answer drops them too, since a changed
                    // `.gitignore` is one of the things that staled it.
                    app.file_tree.gitignore.extend(gitignore);
                    app.file_tree.children.insert(dir, entries);
                }
                // Notify either way — a dropped answer needs a paint to
                // re-request the load that replaces it.
                cx.notify();
            });
        })
        .detach();
    }

    /// Point the search at `query` (empty = not searching), starting a
    /// debounced background walk when it isn't the one already in flight.
    /// Called from render, so the steady state is one string comparison.
    fn sync_search(&mut self, query: &str, roots: &[PathBuf], cx: &mut Context<Tty7App>) {
        let Some(generation) = self.search.retarget(query, self.show_hidden) else {
            return;
        };
        let mut loader = self.loader();
        let (query, roots) = (query.to_string(), roots.to_vec());
        cx.spawn(async move |app, cx| {
            cx.background_executor().timer(SEARCH_DEBOUNCE).await;
            // Another keystroke during the wait retargeted the search: bow out
            // before touching the disk at all, which is the point of the wait.
            let current = app
                .update(cx, |app, _| app.file_tree.search.generation == generation)
                .unwrap_or(false);
            if !current {
                return;
            }
            // The walk's own matchers stay with the walk: it visits up to
            // `MAX_DIRS` directories, and folding all of those into the cache
            // would make every later `loader()` clone a map sized by the search
            // rather than by what's on screen.
            let hits = cx
                .background_executor()
                .spawn(async move { loader.search(&roots, &query) })
                .await;
            let _ = app.update(cx, |app, cx| {
                if app.file_tree.search.accept(generation, hits) {
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// The hits of the last accepted walk, as flat rows. Until one lands this
    /// is empty (or, mid-retype, the previous query's — better than blanking
    /// the list for every keystroke).
    fn search_rows(&self) -> Vec<TreeRow> {
        self.search
            .hits
            .iter()
            .map(|e| TreeRow {
                entry: e.clone(),
                // Flat: a match's own indentation would be meaningless without
                // its ancestors on screen.
                depth: 0,
                is_root: false,
                expanded: false,
            })
            .collect()
    }

    /// Flatten `roots` + `expanded` directories into display order (both come
    /// from the active tab's panel state).
    pub(crate) fn visible_rows(
        &self,
        roots: &[PathBuf],
        expanded: &HashSet<PathBuf>,
    ) -> Vec<TreeRow> {
        let mut rows = Vec::new();
        for root in roots {
            let name = root
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| root.display().to_string());
            rows.push(TreeRow {
                entry: TreeEntry {
                    name,
                    path: root.clone(),
                    is_dir: true,
                    ignored: false,
                },
                depth: 0,
                is_root: true,
                expanded: true,
            });
            self.flatten_dir(root, 1, expanded, &mut rows);
        }
        rows
    }

    fn flatten_dir(
        &self,
        dir: &Path,
        depth: usize,
        expanded: &HashSet<PathBuf>,
        out: &mut Vec<TreeRow>,
    ) {
        let Some(entries) = self.children.get(dir) else {
            return;
        };
        for e in entries {
            if !self.show_hidden && e.name.starts_with('.') {
                continue;
            }
            let is_expanded = e.is_dir && expanded.contains(&e.path);
            out.push(TreeRow {
                entry: e.clone(),
                depth,
                is_root: false,
                expanded: is_expanded,
            });
            if is_expanded {
                self.flatten_dir(&e.path, depth + 1, expanded, out);
            }
        }
    }

    /// Drop cached listings after a filesystem change under `dir`.
    fn invalidate_dir(&mut self, dir: &Path) {
        self.children.remove(dir);
        self.loads.invalidate(dir);
    }

    /// Drop every listing and every compiled matcher — for the changes no
    /// smaller invalidation covers: a `.gitignore` edit (its patterns reach any
    /// depth below it) or a new root set.
    fn invalidate_all(&mut self) {
        self.children.clear();
        self.gitignore.clear();
        self.loads.invalidate_all();
        self.search.restart();
    }
}

// ---------------------------------------------------------------------------
// Pure helpers (tested).
// ---------------------------------------------------------------------------

/// Directories first, then case-insensitive by name (dotfiles keep their
/// leading-dot position in that ordering — they sort before letters).
pub(crate) fn sort_entries(entries: &mut [TreeEntry]) {
    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
}

/// The repository root for a path: the nearest ancestor containing `.git`
/// (dir or worktree file), or `None` outside any repo.
pub(crate) fn repo_root_for(path: &Path) -> Option<PathBuf> {
    path.ancestors()
        .find(|p| p.join(".git").exists())
        .map(Path::to_path_buf)
}

/// Single-quote a path for the shell; embedded `'` becomes `'\''`.
pub(crate) fn shell_quote(path: &Path) -> String {
    let s = path.to_string_lossy();
    if !s.is_empty()
        && s.chars()
            .all(|c| c.is_alphanumeric() || "/.-_~+".contains(c))
    {
        return s.into_owned();
    }
    format!("'{}'", s.replace('\'', r"'\''"))
}

// ---------------------------------------------------------------------------
// Tty7App: toggling, fs events, row operations.
// ---------------------------------------------------------------------------

impl Tty7App {
    /// Derive the root set from the active tab's panes: each pane cwd maps to
    /// its repo root (or itself outside a repo); home as the last resort.
    pub(crate) fn file_tree_refresh_roots(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut roots: Vec<PathBuf> = Vec::new();
        if let Some(tab) = self.tabs.get(self.active) {
            for leaf in tab.pane.leaves() {
                let Some(cwd) = leaf.read(cx).cwd() else {
                    continue;
                };
                let root = repo_root_for(&cwd).unwrap_or(cwd);
                if !roots.contains(&root) {
                    roots.push(root);
                }
            }
        }
        if roots.is_empty()
            && let Some(home) = std::env::var_os("HOME")
        {
            roots.push(PathBuf::from(home));
        }
        let _ = window;
        let Some(code) = self.tab_code_mut_or_init() else {
            return;
        };
        // Dropping the caches is only correct work when the root set actually
        // moved, and doing it unconditionally would spin: render calls this
        // whenever the roots are empty, so a tab that can't produce any (no
        // panes, no `HOME`) would clear the caches and re-notify every frame.
        if roots != code.roots {
            code.roots = roots;
            // Refresh listings but keep expansion state; the caches are shared
            // (path-keyed), so a stale entry only costs a relist.
            self.file_tree.invalidate_all();
            cx.notify();
        }
        // One watcher over every tab's roots — which can move while this tab's
        // stay put (closing a tab takes its roots out of the union), so this
        // compares the union itself rather than riding on the check above.
        let union: HashSet<PathBuf> = self
            .tabs
            .iter()
            .filter_map(|t| t.code.as_deref())
            .flat_map(|c| c.roots.iter().cloned())
            .collect();
        if union != self.file_tree.watched {
            self.file_tree.rebuild_watcher(&union);
        }
    }

    /// Watcher callback (debounced): drop affected listing caches so the next
    /// render relists. A `.gitignore` change resets ignore state wholesale —
    /// its patterns can affect any depth below it.
    pub(crate) fn file_tree_apply_fs_events(
        &mut self,
        paths: &HashSet<PathBuf>,
        cx: &mut Context<Self>,
    ) {
        // The caches are shared across tabs, so invalidate unconditionally —
        // a hidden tab's stale listing would otherwise survive until reopened.
        let gitignore_touched = paths
            .iter()
            .any(|p| p.file_name().is_some_and(|n| n == ".gitignore"));
        if gitignore_touched {
            self.file_tree.invalidate_all();
        } else {
            for p in paths {
                if let Some(parent) = p.parent() {
                    self.file_tree.invalidate_dir(parent);
                }
                // A changed dir itself (e.g. a mkdir) also invalidates its own
                // listing if cached.
                self.file_tree.invalidate_dir(p);
            }
            // Deliberately *not* restarting the search here. Its results are
            // their own walk rather than a view over the listings just dropped,
            // so they do go stale — but restarting on every event batch starves
            // the walk outright: this callback fires roughly every
            // `REFRESH_DEBOUNCE`, and a restart bumps the generation that the
            // walk re-checks after waiting `SEARCH_DEBOUNCE`, so under any
            // sustained churn (a build writing into `target/`, which the
            // watcher reports because it knows nothing about gitignore) every
            // walk bows out before it ever reads a directory and the list stays
            // empty forever. A snapshot that's a few seconds old until the next
            // keystroke is the better failure.
        }
        cx.notify();
    }

    fn file_tree_toggle_expand(&mut self, dir: &Path, cx: &mut Context<Self>) {
        let Some(code) = self.tab_code_mut() else {
            return;
        };
        if !code.expanded.remove(dir) {
            code.expanded.insert(dir.to_path_buf());
        }
        cx.notify();
    }

    /// Row activation (click / Enter): directories toggle, files open in the
    /// editor panel.
    fn file_tree_activate(
        &mut self,
        row_path: &Path,
        is_dir: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(code) = self.tab_code_mut() {
            code.selected = Some(row_path.to_path_buf());
        }
        // Search results are a flat list, so "expand" there has nothing to show.
        // Clicking a directory in them means "take me to it": drop the query and
        // open the real tree down to that directory, which is the only way the
        // click can produce a visible result.
        let searching = !self.file_search.read(cx).value().trim().is_empty();
        if is_dir && searching {
            self.file_tree_reveal(row_path, cx);
            self.file_search
                .update(cx, |st, cx| st.set_value("", window, cx));
            cx.notify();
            return;
        }
        if is_dir {
            self.file_tree_toggle_expand(row_path, cx);
        } else {
            self.open_file_in_editor(row_path, window, cx);
        }
        cx.notify();
    }

    /// Expand `dir` and every ancestor of it up to its root, so a path buried
    /// several levels down becomes visible in one step.
    fn file_tree_reveal(&mut self, dir: &Path, cx: &mut Context<Self>) {
        let roots = self.tab_code().map(|c| c.roots.clone()).unwrap_or_default();
        let Some(root) = roots.iter().find(|r| dir.starts_with(r)).cloned() else {
            return;
        };
        let Some(code) = self.tab_code_mut() else {
            return;
        };
        for a in dir.ancestors().take_while(|a| a.starts_with(&root)) {
            code.expanded.insert(a.to_path_buf());
        }
        cx.notify();
    }

    /// Keyboard navigation over the flattened rows.
    fn file_tree_key_down(
        &mut self,
        ev: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(code) = self.tab_code() else {
            return;
        };
        let rows = self.file_tree.visible_rows(&code.roots, &code.expanded);
        if rows.is_empty() {
            return;
        }
        let sel_ix = code
            .selected
            .as_ref()
            .and_then(|s| rows.iter().position(|r| r.entry.path == *s));
        let key = ev.keystroke.key.as_str();
        match key {
            "up" | "down" => {
                let next = match (sel_ix, key) {
                    (None, _) => 0,
                    (Some(i), "up") => i.saturating_sub(1),
                    (Some(i), _) => (i + 1).min(rows.len() - 1),
                };
                let path = rows[next].entry.path.clone();
                if let Some(code) = self.tab_code_mut() {
                    code.selected = Some(path);
                }
                cx.notify();
            }
            "left" => {
                let Some(i) = sel_ix else { return };
                let row = &rows[i];
                let (path, is_dir, expanded, is_root) = (
                    row.entry.path.clone(),
                    row.entry.is_dir,
                    row.expanded,
                    row.is_root,
                );
                let parent_in_rows = path
                    .parent()
                    .is_some_and(|p| rows.iter().any(|r| r.entry.path == p));
                if let Some(code) = self.tab_code_mut() {
                    if is_dir && expanded && !is_root {
                        code.expanded.remove(&path);
                    } else if parent_in_rows && let Some(parent) = path.parent() {
                        // Jump to the parent row (stay put at a root).
                        code.selected = Some(parent.to_path_buf());
                    }
                }
                cx.notify();
            }
            "right" => {
                let Some(i) = sel_ix else { return };
                let row = &rows[i];
                if row.entry.is_dir && !row.expanded && !row.is_root {
                    let path = row.entry.path.clone();
                    if let Some(code) = self.tab_code_mut() {
                        code.expanded.insert(path);
                    }
                    cx.notify();
                }
            }
            "enter" => {
                let Some(i) = sel_ix else { return };
                let (path, is_dir) = (rows[i].entry.path.clone(), rows[i].entry.is_dir);
                self.file_tree_activate(&path, is_dir, window, cx);
            }
            _ => {}
        }
    }

    // ----- Inline edits (new file / new folder / rename) --------------------

    fn file_tree_begin_edit(
        &mut self,
        edit_for: TreeEditKind,
        target: &Path,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let initial = match edit_for {
            TreeEditKind::Rename => target
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default(),
            _ => String::new(),
        };
        let input = cx.new(|cx| {
            let mut st = InputState::new(window, cx).placeholder(match edit_for {
                TreeEditKind::NewFile => "file name",
                TreeEditKind::NewFolder => "folder name",
                TreeEditKind::Rename => "new name",
            });
            st.set_value(initial, window, cx);
            st
        });
        input.update(cx, |st, cx| st.focus(window, cx));
        let sub = cx.subscribe_in(
            &input,
            window,
            |this: &mut Tty7App, _input, ev, window, cx| match ev {
                InputEvent::PressEnter { .. } => this.file_tree_commit_edit(window, cx),
                InputEvent::Blur => this.file_tree_cancel_edit(cx),
                _ => {}
            },
        );
        self.file_tree.editing_subs = vec![sub];
        // New entries land in the target dir (or the file's parent), which
        // must be expanded for the inline input row to show.
        let host_dir = if target.is_dir() {
            target.to_path_buf()
        } else {
            target.parent().unwrap_or(target).to_path_buf()
        };
        if !matches!(edit_for, TreeEditKind::Rename)
            && let Some(code) = self.tab_code_mut()
        {
            code.expanded.insert(host_dir.clone());
        }
        self.file_tree.editing = Some(match edit_for {
            TreeEditKind::NewFile => TreeEdit::NewFile {
                dir: host_dir,
                input,
            },
            TreeEditKind::NewFolder => TreeEdit::NewFolder {
                dir: host_dir,
                input,
            },
            TreeEditKind::Rename => TreeEdit::Rename {
                path: target.to_path_buf(),
                input,
            },
        });
        cx.notify();
    }

    fn file_tree_cancel_edit(&mut self, cx: &mut Context<Self>) {
        self.file_tree.editing = None;
        self.file_tree.editing_subs.clear();
        cx.notify();
    }

    fn file_tree_commit_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(edit) = self.file_tree.editing.take() else {
            return;
        };
        self.file_tree.editing_subs.clear();
        let name = edit.input().read(cx).value().trim().to_string();
        if name.is_empty() || name.contains('/') {
            cx.notify();
            return;
        }
        let result: std::io::Result<PathBuf> = match &edit {
            TreeEdit::NewFile { dir, .. } => {
                let path = dir.join(&name);
                std::fs::File::create_new(&path).map(|_| path)
            }
            TreeEdit::NewFolder { dir, .. } => {
                let path = dir.join(&name);
                std::fs::create_dir(&path).map(|_| path)
            }
            TreeEdit::Rename { path, .. } => {
                let to = path.with_file_name(&name);
                if to.exists() {
                    Err(std::io::Error::new(
                        std::io::ErrorKind::AlreadyExists,
                        "target exists",
                    ))
                } else {
                    std::fs::rename(path, &to).map(|_| to)
                }
            }
        };
        match result {
            Ok(new_path) => {
                self.file_tree.invalidate_dir(edit.host_dir());
                if let Some(code) = self.tab_code_mut() {
                    code.selected = Some(new_path.clone());
                }
                // A freshly created file opens straight into the editor.
                if matches!(edit, TreeEdit::NewFile { .. }) {
                    self.open_file_in_editor(&new_path, window, cx);
                }
            }
            Err(e) => {
                use gpui_component::WindowExt as _;
                window.push_notification(format!("{e}"), cx);
            }
        }
        cx.notify();
    }

    /// Context-menu delete, with a native confirm (recursive for dirs).
    fn file_tree_delete(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| path.display().to_string());
        let is_dir = path.is_dir();
        let detail = if is_dir {
            "The folder and everything inside it will be deleted."
        } else {
            "The file will be deleted."
        };
        let answer = window.prompt(
            PromptLevel::Warning,
            &format!("Delete \"{name}\"?"),
            Some(detail),
            &["Delete", "Cancel"],
            cx,
        );
        cx.spawn_in(window, async move |app, cx| {
            let Ok(0) = answer.await else { return };
            let _ = app.update_in(cx, |app, window, cx| {
                let result = if is_dir {
                    std::fs::remove_dir_all(&path)
                } else {
                    std::fs::remove_file(&path)
                };
                match result {
                    Ok(()) => {
                        if let Some(parent) = path.parent() {
                            app.file_tree.invalidate_dir(parent);
                        }
                        if let Some(code) = app.tab_code_mut()
                            && code.selected.as_deref() == Some(&path)
                        {
                            code.selected = None;
                        }
                        cx.notify();
                    }
                    Err(e) => {
                        use gpui_component::WindowExt as _;
                        window.push_notification(format!("Delete failed: {e}"), cx);
                    }
                }
            });
        })
        .detach();
    }

    /// "cd here": type `cd <dir>` + Enter into the focused pane's PTY.
    fn file_tree_cd(&mut self, dir: &Path, window: &mut Window, cx: &mut Context<Self>) {
        let Some(leaf) = self
            .tabs
            .get(self.active)
            .and_then(|t| t.pane.focused_or_first(window, cx))
        else {
            return;
        };
        leaf.read(cx)
            .run_command_line(&format!("cd {}", shell_quote(dir)));
        self.focus_active(window, cx);
    }

    /// "Attach to agent": paste an `@path` reference into the pane running a
    /// coding agent (unsubmitted, so the user can keep typing the prompt).
    fn file_tree_attach_to_agent(&mut self, path: &Path, cx: &mut Context<Self>) {
        let Some(target) = self.agent_target_leaf(cx) else {
            crate::terminal::notify_desktop(
                Some("tty7"),
                "No running coding agent found — start one (claude, codex, …) in a pane first.",
            );
            return;
        };
        // Prefer a repo-relative path (what agents resolve best) when the file
        // sits under one of the tree's roots.
        let rel = self
            .tab_code()
            .into_iter()
            .flat_map(|c| c.roots.iter())
            .find_map(|r| path.strip_prefix(r).ok())
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| path.to_path_buf());
        target.update(cx, |view, cx| {
            view.paste(format!("@{} ", rel.display()), cx);
        });
    }
}

/// Which inline edit a context-menu entry starts.
#[derive(Clone, Copy)]
enum TreeEditKind {
    NewFile,
    NewFolder,
    Rename,
}

// ---------------------------------------------------------------------------
// Rendering.
// ---------------------------------------------------------------------------

impl Tty7App {
    /// The file-tree column: the code overlay's left side (the overlay
    /// renders the divider and the editor to its right).
    /// Just the scrolling rows of the tree — no header, no fixed width, no
    /// surface of its own — so a host that already has those (the right detail
    /// panel) can drop the tree into its own column. Shares every bit of state
    /// with [`render_file_tree_column`]: same roots, same expand set, same
    /// click-to-open, so the panel and the code overlay are two views of one tree.
    pub(crate) fn render_file_tree_rows(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let roots_empty = self.tab_code().map(|c| c.roots.is_empty()).unwrap_or(true);
        // The tree is normally rooted when the code panel opens; the right panel
        // can be the first thing to ask for it, so root it here too when empty.
        if roots_empty {
            self.file_tree_refresh_roots(window, cx);
        }
        let (roots, expanded) = match self.tab_code() {
            Some(code) => (code.roots.clone(), code.expanded.clone()),
            None => (Vec::new(), std::collections::HashSet::new()),
        };
        let query = self.file_search.read(cx).value().trim().to_lowercase();
        // Both branches only read caches; whatever is missing is queued for the
        // background executor and shows up on the paint after it lands.
        self.file_tree.sync_search(&query, &roots, cx);
        let rows = if query.is_empty() {
            self.file_tree.request_loads(&roots, &expanded, cx);
            self.file_tree.visible_rows(&roots, &expanded)
        } else {
            self.file_tree.search_rows()
        };
        let column = v_flex()
            .id("right-panel-tree-rows")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .track_scroll(&self.right_panel.tree_scroll)
            .px_1()
            .pb_1()
            // Keyboard nav (arrows / enter / rename) followed the tree out of the
            // overlay: the rows still own the focus handle its key handler reads.
            .track_focus(&self.file_tree.focus_handle)
            .on_key_down(cx.listener(|this, ev: &KeyDownEvent, window, cx| {
                this.file_tree_key_down(ev, window, cx);
            }))
            .children(
                rows.iter()
                    .flat_map(|row| self.render_tree_row(row, window, cx)),
            );
        crate::ui::scrollbar::with_vertical_scrollbar(
            "right-panel-tree-scrollbar",
            column,
            &self.right_panel.tree_scroll,
        )
    }

    /// One row (plus, when an inline edit targets it, the edit input row).
    fn render_tree_row(
        &self,
        row: &TreeRow,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let path = row.entry.path.clone();
        let is_dir = row.entry.is_dir;
        let selected = self.tab_code().and_then(|c| c.selected.as_deref()) == Some(&*path);
        let muted = cx.theme().muted_foreground;
        let sf = cx.global::<crate::ui::presets::Surfaces>().popover;
        // Unsaved edits used to be visible on the editor's file tabs; with those
        // gone the tree is the only place an open buffer is represented, so it has
        // to carry the dirty marker or unsaved work becomes invisible.
        let dirty = self
            .tab_code()
            .is_some_and(|c| c.files.iter().any(|f| f.dirty && f.path == *path));

        // Inline rename replaces the row's label with an input.
        let renaming = matches!(
            &self.file_tree.editing,
            Some(TreeEdit::Rename { path: p, .. }) if *p == path
        );

        let icon = if row.is_root {
            IconName::FolderOpen
        } else if is_dir {
            if row.expanded {
                IconName::FolderOpen
            } else {
                IconName::Folder
            }
        } else {
            IconName::File
        };

        let label: AnyElement = if renaming {
            let input = self.file_tree.editing.as_ref().unwrap().input().clone();
            Input::new(&input).xsmall().into_any_element()
        } else {
            div()
                .flex_1()
                .min_w_0()
                .text_ellipsis()
                .text_sm()
                .when(row.entry.ignored, |d| {
                    d.italic().text_color(muted.opacity(0.7))
                })
                .when(row.is_root, |d| d.font_weight(gpui::FontWeight::MEDIUM))
                .child(SharedString::from(row.entry.name.clone()))
                .into_any_element()
        };

        let row_el = h_flex()
            .id(SharedString::from(format!("tree-{}", path.display())))
            .items_center()
            .gap_1()
            .pl(px(6.0 + row.depth as f32 * INDENT))
            .pr_1()
            .py_1()
            .rounded(cx.theme().radius)
            .cursor_pointer()
            // Soft inset-pill highlight on the content surface. The tree paints on
            // `popover` (see the container below), so this is that surface's
            // ladder — read explicitly rather than through `Theme::accent`, which
            // is gpui-component's name for a row highlight and says nothing about
            // which surface it was anchored to. Hover was `accent.opacity(0.5)`; a
            // ladder rung is a real colour, so it doesn't change meaning depending
            // on what it lands on.
            .when(selected, |d| d.bg(gpui::rgb(sf.selected)))
            .when(!selected, |d| d.hover(|s| s.bg(gpui::rgb(sf.hover))))
            // Folders take the full foreground, files the muted tone — a neutral
            // weight difference, no hue, so the tree keeps the terminal's calm.
            .child(Icon::new(icon).xsmall().text_color(if is_dir {
                cx.theme().foreground
            } else {
                muted
            }))
            .child(label)
            .when(dirty, |d| {
                d.child(
                    div()
                        .flex_none()
                        .size(px(6.))
                        .rounded_full()
                        .bg(cx.theme().warning),
                )
            })
            .on_mouse_down(
                MouseButton::Left,
                cx.listener({
                    let path = path.clone();
                    move |this, _, window, cx| {
                        this.file_tree.focus_handle.focus(window, cx);
                        this.file_tree_activate(&path, is_dir, window, cx);
                    }
                }),
            )
            // Drag the row as external paths — the terminal's existing drop
            // handler shell-escapes and inserts them.
            .on_drag(ExternalPaths(vec![path.clone()].into()), {
                let name = row.entry.name.clone();
                move |_, _, _, cx| {
                    let name = name.clone();
                    cx.new(|_| DragGhost { name })
                }
            })
            .context_menu({
                let app = cx.entity().downgrade();
                let path = path.clone();
                let is_root = row.is_root;
                let show_hidden = self.file_tree.show_hidden;
                move |menu, _window, cx| {
                    let danger = cx.theme().danger;
                    Self::tree_row_context_menu(
                        menu,
                        &path,
                        is_dir,
                        is_root,
                        show_hidden,
                        danger,
                        &app,
                    )
                }
            });

        let mut out: Vec<AnyElement> = vec![row_el.into_any_element()];

        // New-file/new-folder edit input renders as a pseudo-child row of its
        // host directory (right after the dir's own row).
        if let Some(edit) = &self.file_tree.editing {
            let host_matches = match edit {
                TreeEdit::NewFile { dir, .. } | TreeEdit::NewFolder { dir, .. } => *dir == path,
                TreeEdit::Rename { .. } => false,
            };
            if host_matches {
                let input = edit.input().clone();
                out.push(
                    h_flex()
                        .items_center()
                        .gap_1()
                        .pl(px(6.0 + (row.depth + 1) as f32 * INDENT))
                        .pr_1()
                        .py_0p5()
                        .child(Input::new(&input).xsmall())
                        .into_any_element(),
                );
            }
        }
        out
    }

    /// The per-row right-click menu, mirroring Warp's Project Explorer set, plus
    /// the tree's one view option (dotfiles) — which lives here rather than as a
    /// header button: it is set once and then forgotten, and a tile in the header
    /// spends the panel's scarcest row on it forever.
    fn tree_row_context_menu(
        menu: PopupMenu,
        path: &Path,
        is_dir: bool,
        is_root: bool,
        show_hidden: bool,
        danger: gpui::Hsla,
        app: &gpui::WeakEntity<Self>,
    ) -> PopupMenu {
        let mut menu = menu.min_w(px(200.));
        let p = path.to_path_buf();

        if !is_dir {
            menu = menu.item(PopupMenuItem::new("Open").on_click({
                let app = app.clone();
                let p = p.clone();
                move |_, window, cx| {
                    let _ = app.update(cx, |this, cx| this.open_file_in_editor(&p, window, cx));
                }
            }));
        }
        if is_dir {
            menu = menu.item(PopupMenuItem::new("cd Here").on_click({
                let app = app.clone();
                let p = p.clone();
                move |_, window, cx| {
                    let _ = app.update(cx, |this, cx| this.file_tree_cd(&p, window, cx));
                }
            }));
        }
        menu = menu
            .item(PopupMenuItem::new("Insert Path in Terminal").on_click({
                let app = app.clone();
                let p = p.clone();
                move |_, window, cx| {
                    let _ = app.update(cx, |this, cx| {
                        if let Some(leaf) = this
                            .tabs
                            .get(this.active)
                            .and_then(|t| t.pane.focused_or_first(window, cx))
                        {
                            leaf.update(cx, |view, cx| view.paste(shell_quote(&p), cx));
                        }
                    });
                }
            }))
            .item(PopupMenuItem::new("Attach to Agent").on_click({
                let app = app.clone();
                let p = p.clone();
                move |_, _window, cx| {
                    let _ = app.update(cx, |this, cx| this.file_tree_attach_to_agent(&p, cx));
                }
            }))
            .separator()
            .item(PopupMenuItem::new("New File").on_click({
                let app = app.clone();
                let p = p.clone();
                move |_, window, cx| {
                    let _ = app.update(cx, |this, cx| {
                        this.file_tree_begin_edit(TreeEditKind::NewFile, &p, window, cx)
                    });
                }
            }))
            .item(PopupMenuItem::new("New Folder").on_click({
                let app = app.clone();
                let p = p.clone();
                move |_, window, cx| {
                    let _ = app.update(cx, |this, cx| {
                        this.file_tree_begin_edit(TreeEditKind::NewFolder, &p, window, cx)
                    });
                }
            }));

        if !is_root {
            menu = menu.item(PopupMenuItem::new("Rename").on_click({
                let app = app.clone();
                let p = p.clone();
                move |_, window, cx| {
                    let _ = app.update(cx, |this, cx| {
                        this.file_tree_begin_edit(TreeEditKind::Rename, &p, window, cx)
                    });
                }
            }));
        }

        menu = menu
            .separator()
            .item(PopupMenuItem::new("Copy Path").on_click({
                let p = p.clone();
                move |_, _window, cx| {
                    cx.write_to_clipboard(gpui::ClipboardItem::new_string(p.display().to_string()));
                }
            }))
            .item(PopupMenuItem::new("Reveal in Finder").on_click({
                let p = p.clone();
                move |_, _window, cx| {
                    cx.reveal_path(&p);
                }
            }));

        menu = menu.separator().item(dotfiles_menu_item(show_hidden, app));

        if !is_root {
            menu = menu.separator().item(
                PopupMenuItem::element(move |_window, _cx| {
                    div().text_color(danger).child("Delete")
                })
                .on_click({
                    let app = app.clone();
                    move |_, window, cx| {
                        let p = p.clone();
                        let _ = app.update(cx, |this, cx| this.file_tree_delete(p, window, cx));
                    }
                }),
            );
        }
        menu
    }
}

/// The tree's dotfile switch as a row of the tree's existing right-click menu.
///
/// The label states what the click will do rather than checking off the current
/// state: a single checked item makes `PopupMenu` reserve a left icon gutter on
/// *every* row in the menu (see `tab_strip::window_chrome`), and the menu has a
/// dozen rows with nothing to put in one.
fn dotfiles_menu_item(show_hidden: bool, app: &gpui::WeakEntity<Tty7App>) -> PopupMenuItem {
    let app = app.clone();
    PopupMenuItem::new(if show_hidden {
        "Hide Dotfiles"
    } else {
        "Show Dotfiles"
    })
    .on_click(move |_, _window, cx| {
        let _ = app.update(cx, |this, cx| {
            this.file_tree.show_hidden = !this.file_tree.show_hidden;
            cx.notify();
        });
    })
}

/// The little drag ghost shown while a row is dragged toward a terminal.
struct DragGhost {
    name: String,
}

impl gpui::Render for DragGhost {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .items_center()
            .gap_1()
            .px_2()
            .py_1()
            .rounded(cx.theme().radius)
            .bg(cx.theme().popover)
            .border_1()
            .border_color(cx.theme().border)
            .text_sm()
            .child(Icon::new(IconName::File).xsmall())
            .child(SharedString::from(self.name.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, is_dir: bool) -> TreeEntry {
        TreeEntry {
            name: name.to_string(),
            path: PathBuf::from(format!("/x/{name}")),
            is_dir,
            ignored: false,
        }
    }

    #[test]
    fn sort_puts_dirs_first_then_case_insensitive_names() {
        let mut v = vec![
            entry("zeta.rs", false),
            entry("Alpha", true),
            entry("beta", true),
            entry("Apple.rs", false),
        ];
        sort_entries(&mut v);
        let names: Vec<&str> = v.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["Alpha", "beta", "Apple.rs", "zeta.rs"]);
    }

    #[test]
    fn shell_quote_leaves_safe_paths_and_quotes_the_rest() {
        assert_eq!(shell_quote(Path::new("/a/b.txt")), "/a/b.txt");
        assert_eq!(shell_quote(Path::new("/a dir/f")), "'/a dir/f'");
        assert_eq!(shell_quote(Path::new("/a'b")), r"'/a'\''b'");
    }

    #[test]
    fn repo_root_walks_up_to_git() {
        let tmp = std::env::temp_dir().join(format!("tty7-tree-test-{}", std::process::id()));
        let nested = tmp.join("repo/src/deep");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::create_dir_all(tmp.join("repo/.git")).unwrap();
        assert_eq!(repo_root_for(&nested), Some(tmp.join("repo")));
        assert_eq!(repo_root_for(&tmp), None);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    // A load that a watcher event overtook must not install its answer: the
    // listing it read predates the change that invalidated it.
    #[test]
    fn a_load_invalidated_mid_flight_drops_its_answer() {
        let dir = PathBuf::from("/x/src");
        let mut loads = Loads::default();
        assert!(loads.begin(&dir), "first request spawns");
        assert!(!loads.begin(&dir), "a repaint must not spawn a second load");
        loads.invalidate(&dir);
        assert!(!loads.finish(&dir), "the stale answer is thrown away");
        assert!(loads.begin(&dir), "and the next paint may ask again");
        assert!(loads.finish(&dir), "an untouched load installs its answer");
    }

    // Invalidating a directory with nothing in flight must not poison the load
    // that comes after it — that would drop every answer following any event.
    #[test]
    fn invalidating_an_idle_directory_does_not_stale_the_next_load() {
        let dir = PathBuf::from("/x/src");
        let mut loads = Loads::default();
        loads.invalidate(&dir);
        assert!(loads.begin(&dir));
        assert!(loads.finish(&dir));
    }

    #[test]
    fn search_retarget_spawns_once_per_query_and_older_walks_lose() {
        let mut search = SearchState::default();
        let first = search.retarget("fo", false).expect("a new query walks");
        assert!(
            search.retarget("fo", false).is_none(),
            "a repaint mid-walk must not queue a second one"
        );
        let second = search
            .retarget("foo", false)
            .expect("a changed query walks");
        assert_ne!(first, second);

        assert!(
            !search.accept(first, vec![entry("stale.rs", false)]),
            "the overtaken walk's hits are dropped"
        );
        assert!(search.accept(second, vec![entry("foo.rs", false)]));
        assert_eq!(search.hits.len(), 1);

        // The eye toggle filters hits inside the walk, so flipping it re-walks
        // the query that's already on screen.
        let third = search
            .retarget("foo", true)
            .expect("showing dotfiles re-walks");
        assert_ne!(second, third);
        assert!(search.retarget("foo", true).is_none());

        // Clearing the box drops the hits so the next query can't flash them,
        // and a restart re-walks the same query rather than sitting on it.
        assert!(search.retarget("", true).is_none());
        assert!(search.hits.is_empty());
        search.retarget("foo", true).expect("typing again walks");
        search.restart();
        assert!(search.retarget("foo", true).is_some(), "restart re-walks");
    }

    // The gitignore chain moved onto the background loader with the matchers
    // behind `Arc`; the semantics it has to keep are deepest-match-wins,
    // whitelist un-ignore, and `.git` ignored whatever the patterns say.
    #[test]
    fn loader_tags_ignored_entries_down_the_gitignore_chain() {
        let tmp = std::env::temp_dir().join(format!("tty7-tree-ignore-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join(".git")).unwrap();
        std::fs::create_dir_all(tmp.join("src")).unwrap();
        std::fs::write(tmp.join(".gitignore"), "*.log\nbuild/\n").unwrap();
        // The deeper file un-ignores one of the parent's patterns.
        std::fs::write(tmp.join("src/.gitignore"), "!keep.log\n").unwrap();
        std::fs::write(tmp.join("drop.log"), "").unwrap();
        std::fs::write(tmp.join("src/keep.log"), "").unwrap();
        std::fs::write(tmp.join("src/main.rs"), "").unwrap();

        let mut loader = TreeLoader {
            gitignore: HashMap::new(),
            show_hidden: false,
        };
        let ignored = |entries: &[TreeEntry], name: &str| {
            entries
                .iter()
                .find(|e| e.name == name)
                .unwrap_or_else(|| panic!("{name} missing"))
                .ignored
        };
        let top = loader.list_dir(&tmp, &tmp);
        assert!(ignored(&top, "drop.log"));
        assert!(ignored(&top, ".git"));
        assert!(!ignored(&top, "src"));
        let nested = loader.list_dir(&tmp.join("src"), &tmp);
        assert!(!ignored(&nested, "keep.log"), "whitelist un-ignores");
        assert!(!ignored(&nested, "main.rs"));
        // The matchers it compiled ride back to the UI thread's cache.
        assert_eq!(loader.gitignore.len(), 2);

        let hits = loader.search(std::slice::from_ref(&tmp), "log");
        let names: Vec<&str> = hits.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["keep.log"], "ignored hits stay out of search");
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
