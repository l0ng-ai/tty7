use std::borrow::Cow;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::core::config::RightPanelTab;
use crate::core::git::status::{DecoStatus, DirRollup, StatusIndex};
use crate::terminal::git_data::index_of;
use crate::ui::app::Tty7App;
use crate::ui::file_copy;
use crate::ui::host_ops::{ByHost, HostId, HostOps, InFlight, SharedHost, WatchSub};
use crate::ui::host_registry::HostRegistry;
use crate::ui::i18n::{L10nKey, t, t_fmt};
use crate::ui::right_panel::git_badge;
use crate::ui::scm::status::{status_color, status_glyph};
use gpui::prelude::*;
use gpui::{
    AnyElement, App, Context, Entity, ExternalPaths, FocusHandle, KeyDownEvent, MouseButton,
    PromptLevel, SharedString, Subscription, Window, div, px,
};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::menu::{ContextMenuExt as _, PopupMenu, PopupMenuItem};
use gpui_component::{ActiveTheme as _, Icon, IconName, Sizable as _, h_flex, v_flex};

const INDENT: f32 = 14.0;

const REFRESH_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(200);

const SEARCH_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(200);

const SEARCH_LIMIT: usize = 200;

const SEARCH_MAX_DIRS: usize = 2000;

#[derive(Clone, PartialEq)]
pub(crate) struct TreeEntry {
    pub name: String,
    pub path: PathBuf,
    pub is_dir: bool,
    pub ignored: bool,
}

struct Landed {
    superseded: bool,
    changed: bool,
}

pub(crate) struct TreeRow {
    pub entry: TreeEntry,
    pub depth: usize,
    pub is_root: bool,
    pub expanded: bool,
    /// Stands in for the children of `entry` when there are none to draw.
    /// Without it, a directory still being listed, one that is genuinely empty,
    /// one whose contents are all hidden, and one the OS refused to read all
    /// render as the same nothing.
    pub note: Option<TreeNote>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum TreeNote {
    Loading,
    Empty,
    HiddenOnly,
    Unreadable,
    /// The search stopped at `SEARCH_LIMIT`; the list is a prefix, not the
    /// whole answer, and has to say so.
    SearchCapped,
    /// The search never ran to an answer — the host refused it or the link to
    /// it went away. An empty list here means nothing at all.
    SearchFailed,
}

/// `landed` is how many entries the listing returned, or `None` when nothing
/// has come back yet — which is the whole difference between "empty" and
/// "still working". A non-zero count with no visible rows means the hidden
/// filter took them all, and calling that "empty" is a lie the user can
/// disprove with one keystroke.
fn dir_note(unreadable: bool, landed: Option<usize>) -> TreeNote {
    match (unreadable, landed) {
        (true, _) => TreeNote::Unreadable,
        (false, None) => TreeNote::Loading,
        (false, Some(0)) => TreeNote::Empty,
        (false, Some(_)) => TreeNote::HiddenOnly,
    }
}

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

    fn host_dir(&self) -> &Path {
        match self {
            TreeEdit::NewFile { dir, .. } | TreeEdit::NewFolder { dir, .. } => dir,
            TreeEdit::Rename { path, .. } => path.parent().unwrap_or(path),
        }
    }
}

type DirKey = (HostId, PathBuf);

#[derive(Default)]
struct SearchState {
    generation: u64,
    pending: String,
    hidden: bool,
    hits: Vec<TreeEntry>,
    /// Whether the last search came back as a failure rather than as no hits.
    /// The two used to print the same "Nothing matches …", which is the same
    /// lie `unreadable` was added to stop a directory listing from telling.
    failed: bool,
}

impl SearchState {
    fn retarget(&mut self, query: &str, show_hidden: bool) -> Option<u64> {
        if self.pending == query && self.hidden == show_hidden {
            return None;
        }
        self.generation += 1;
        self.pending = query.to_string();
        self.hidden = show_hidden;
        if query.is_empty() {
            self.hits.clear();
            self.failed = false;
            return None;
        }
        Some(self.generation)
    }

    fn accept(&mut self, generation: u64, ok: bool, hits: Vec<TreeEntry>) -> bool {
        if self.generation != generation {
            return false;
        }
        self.hits = hits;
        self.failed = !ok;
        true
    }

    fn restart(&mut self) {
        self.generation += 1;
        self.pending.clear();
        self.failed = false;
    }
}

pub(crate) struct FileTreeState {
    children: ByHost<PathBuf, Vec<TreeEntry>>,
    loads: InFlight<DirKey>,
    /// Directories whose last listing came back as a failure rather than as an
    /// empty result. `read_dir` used to be `unwrap_or_default()`ed, so a
    /// permission-denied folder was indistinguishable from an empty one.
    unreadable: HashSet<DirKey>,
    stale: HashSet<DirKey>,
    repo_roots: ByHost<PathBuf, PathBuf>,
    repo_root_loads: InFlight<DirKey>,
    search: SearchState,
    pub(crate) show_hidden: bool,
    pub(crate) editing: Option<TreeEdit>,
    editing_subs: Vec<Subscription>,
    watch: Option<Arc<WatchSub>>,
    watch_host: Option<SharedHost>,
    watch_opening: bool,
    watch_busy: bool,
    watch_dirty: bool,
    watched: HashSet<PathBuf>,
    events_tx: smol::channel::Sender<(HostId, Vec<PathBuf>)>,
    pub(crate) focus_handle: FocusHandle,
}

impl FileTreeState {
    pub(crate) fn new(window: &mut Window, cx: &mut Context<Tty7App>) -> Self {
        let (tx, rx) = smol::channel::unbounded::<(HostId, Vec<PathBuf>)>();
        cx.spawn_in(window, async move |app, cx| {
            while let Ok((host, first)) = rx.recv().await {
                cx.background_executor().timer(REFRESH_DEBOUNCE).await;
                let mut changed: HashSet<PathBuf> = first.into_iter().collect();
                while let Ok((h, more)) = rx.try_recv() {
                    if h == host {
                        changed.extend(more);
                    }
                }
                let ok = app.update(cx, |app, cx| {
                    app.file_tree_apply_fs_events(host, &changed, cx);
                });
                if ok.is_err() {
                    break;
                }
            }
        })
        .detach();
        Self {
            watch_host: None,
            children: ByHost::default(),
            loads: InFlight::default(),
            unreadable: HashSet::new(),
            stale: HashSet::new(),
            repo_roots: ByHost::default(),
            repo_root_loads: InFlight::default(),
            search: SearchState::default(),
            show_hidden: false,
            editing: None,
            editing_subs: Vec::new(),
            watch: None,
            watch_opening: false,
            watch_busy: false,
            watch_dirty: false,
            watched: HashSet::new(),
            events_tx: tx,
            focus_handle: cx.focus_handle(),
        }
    }

    fn sync_watch(&mut self, host: SharedHost, dirs: HashSet<PathBuf>, cx: &mut Context<Tty7App>) {
        self.watched = dirs;
        let want: Vec<PathBuf> = self.watched.iter().cloned().collect();
        if !self
            .watch_host
            .as_ref()
            .is_some_and(|opened_with| Arc::ptr_eq(opened_with, &host))
        {
            self.watch = None;
            self.watch_host = None;
            self.watch_busy = false;
            self.watch_dirty = false;
        }
        if let Some(sub) = self.watch.clone() {
            if self.watch_busy {
                self.watch_dirty = true;
                return;
            }
            self.watch_busy = true;
            HostOps::run(
                host,
                cx,
                move |_| sub.set_dirs(&want),
                |app: &mut Tty7App, result: std::io::Result<()>, cx| {
                    app.file_tree.watch_busy = false;
                    if let Err(e) = result {
                        log::warn!("file tree: could not update the watched set: {e}");
                    }
                    if std::mem::take(&mut app.file_tree.watch_dirty) {
                        let want = app.file_tree.watched.clone();
                        let Some(host) = app.active_host(cx) else {
                            return;
                        };
                        app.file_tree.sync_watch(host, want, cx);
                    }
                },
            );
            return;
        }
        if self.watch_opening {
            return;
        }
        self.watch_opening = true;
        let host_id = host.id();
        let opened_host = Arc::clone(&host);
        let opened_with = self.watched.clone();
        HostOps::run(
            host,
            cx,
            {
                let want = want.clone();
                move |h| h.watch(&want).map(Arc::new)
            },
            move |app, result: std::io::Result<Arc<WatchSub>>, cx| {
                app.file_tree.watch_opening = false;
                let sub = match result {
                    Ok(sub) => sub,
                    Err(e) => {
                        log::warn!("file tree: watcher unavailable: {e}");
                        return;
                    }
                };
                let events = sub.events().clone();
                app.file_tree.watch = Some(sub);
                app.file_tree.watch_host = Some(opened_host);
                cx.spawn(async move |app, cx| {
                    while let Ok(batch) = events.recv().await {
                        let ok = app.update(cx, |app, _cx| {
                            let _ = app.file_tree.events_tx.try_send((host_id, batch));
                        });
                        if ok.is_err() {
                            break;
                        }
                    }
                })
                .detach();
                if app.file_tree.watched != opened_with {
                    let want = app.file_tree.watched.clone();
                    let Some(host) = app.active_host(cx) else {
                        return;
                    };
                    app.file_tree.sync_watch(host, want, cx);
                }
            },
        );
    }

    fn request_loads(
        &mut self,
        host: &SharedHost,
        roots: &[PathBuf],
        expanded: &HashSet<PathBuf>,
        cx: &mut Context<Tty7App>,
    ) {
        for root in roots {
            self.request_load(host, root.clone(), root.clone(), cx);
            for dir in expanded {
                if dir.starts_with(root) {
                    self.request_load(host, dir.clone(), root.clone(), cx);
                }
            }
        }
    }

    fn request_load(
        &mut self,
        host: &SharedHost,
        dir: PathBuf,
        root: PathBuf,
        cx: &mut Context<Tty7App>,
    ) {
        let id = host.id();
        let key: DirKey = (id, dir.clone());
        let current = self.children.get(id, &dir).is_some() && !self.stale.contains(&key);
        if current || !self.loads.begin(key.clone()) {
            return;
        }
        self.spawn_load(host, dir, root, cx);
    }

    fn spawn_load(
        &mut self,
        host: &SharedHost,
        dir: PathBuf,
        root: PathBuf,
        cx: &mut Context<Tty7App>,
    ) {
        let id = host.id();
        let key: DirKey = (id, dir.clone());
        HostOps::run(
            host.clone(),
            cx,
            {
                let dir = dir.clone();
                let root = root.clone();
                move |h| {
                    let listing = h.read_dir(&dir, Some(&root));
                    let readable = listing.is_ok();
                    let entries = listing
                        .unwrap_or_default()
                        .into_iter()
                        .map(|e| TreeEntry {
                            path: h.join(&dir, &e.name),
                            name: e.name,
                            is_dir: e.is_dir,
                            ignored: e.ignored,
                        })
                        .collect::<Vec<_>>();
                    (readable, entries)
                }
            },
            move |app, (readable, entries), cx| {
                if readable {
                    app.file_tree.unreadable.remove(&key);
                } else {
                    app.file_tree.unreadable.insert(key.clone());
                }
                let landed = app.file_tree.land_load(&key, id, dir.clone(), entries);
                if landed.changed {
                    cx.notify();
                }
                if !landed.superseded {
                    return;
                }
                let Some(host) = app.active_host(cx) else {
                    return;
                };
                if host.id() != id {
                    return;
                }
                app.file_tree.loads.begin(key);
                app.file_tree.spawn_load(&host, dir, root, cx);
            },
        );
    }

    fn land_load(
        &mut self,
        key: &DirKey,
        id: HostId,
        dir: PathBuf,
        entries: Vec<TreeEntry>,
    ) -> Landed {
        let changed = self.children.get(id, &dir) != Some(&entries);
        let superseded = land_listing(
            &mut self.loads,
            &mut self.children,
            &mut self.stale,
            key,
            id,
            dir,
            entries,
        );
        Landed {
            superseded,
            changed,
        }
    }

    fn sync_search(&mut self, query: &str, roots: &[PathBuf], cx: &mut Context<Tty7App>) {
        let Some(generation) = self.search.retarget(query, self.show_hidden) else {
            return;
        };
        let show_hidden = self.show_hidden;
        let (query, roots) = (query.to_string(), roots.to_vec());
        cx.spawn(async move |app, cx| {
            cx.background_executor().timer(SEARCH_DEBOUNCE).await;
            let _ = app.update(cx, |app, cx| {
                if app.file_tree.search.generation != generation {
                    return;
                }
                let Some(host) = app.active_host(cx) else {
                    return;
                };
                HostOps::run(
                    host,
                    cx,
                    move |h| {
                        // `(ok, hits)` the way `spawn_load` reports a listing:
                        // a search the host refused is not a search with no
                        // hits, and the column has to be able to tell them
                        // apart before it says "Nothing matches".
                        let found =
                            h.search(&roots, &query, SEARCH_LIMIT, SEARCH_MAX_DIRS, show_hidden);
                        let ok = found.is_ok();
                        let hits = found
                            .unwrap_or_default()
                            .into_iter()
                            .map(|hit| TreeEntry {
                                name: hit.name,
                                path: hit.path,
                                is_dir: hit.is_dir,
                                ignored: hit.ignored,
                            })
                            .collect::<Vec<_>>();
                        (ok, hits)
                    },
                    move |app, (ok, hits), cx| {
                        if app.file_tree.search.accept(generation, ok, hits) {
                            cx.notify();
                        }
                    },
                );
            });
        })
        .detach();
    }

    fn search_rows(&self) -> Vec<TreeRow> {
        search_rows(&self.search)
    }
}

/// The rows a search puts in the column, and the note that stands for whatever
/// they do not say by themselves.
fn search_rows(search: &SearchState) -> Vec<TreeRow> {
    let mut rows: Vec<TreeRow> = search
        .hits
        .iter()
        .map(|e| TreeRow {
            entry: e.clone(),
            depth: 0,
            is_root: false,
            expanded: false,
            note: None,
        })
        .collect();
    let note = if search.failed {
        // Ahead of the cap: a failed search has no hits to have capped, and
        // this is the one thing worth saying about it.
        Some(TreeNote::SearchFailed)
    } else if rows.len() >= SEARCH_LIMIT {
        Some(TreeNote::SearchCapped)
    } else {
        None
    };
    if let Some(note) = note {
        rows.push(TreeRow {
            entry: TreeEntry {
                name: String::new(),
                path: PathBuf::new(),
                is_dir: false,
                ignored: false,
            },
            depth: 0,
            is_root: false,
            expanded: false,
            note: Some(note),
        });
    }
    rows
}

impl FileTreeState {
    pub(crate) fn visible_rows(
        &self,
        host: HostId,
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
                note: None,
            });
            self.flatten_dir(host, root, 1, expanded, &mut rows);
        }
        rows
    }

    fn flatten_dir(
        &self,
        host: HostId,
        dir: &Path,
        depth: usize,
        expanded: &HashSet<PathBuf>,
        out: &mut Vec<TreeRow>,
    ) {
        let Some(entries) = self.children.get(host, &dir.to_path_buf()) else {
            out.push(self.note_row(host, dir, depth, None));
            return;
        };
        let mut shown = 0usize;
        for e in entries {
            if !self.show_hidden && e.name.starts_with('.') {
                continue;
            }
            shown += 1;
            let is_expanded = e.is_dir && expanded.contains(&e.path);
            out.push(TreeRow {
                entry: e.clone(),
                depth,
                is_root: false,
                expanded: is_expanded,
                note: None,
            });
            if is_expanded {
                self.flatten_dir(host, &e.path, depth + 1, expanded, out);
            }
        }
        if shown == 0 {
            out.push(self.note_row(host, dir, depth, Some(entries.len())));
        }
    }

    /// Why an expanded directory drew no children. `landed` is the number of
    /// entries the listing actually returned, or `None` when nothing has landed
    /// yet — that is the difference between "empty" and "still working".
    fn note_row(&self, host: HostId, dir: &Path, depth: usize, landed: Option<usize>) -> TreeRow {
        let key: DirKey = (host, dir.to_path_buf());
        let note = dir_note(self.unreadable.contains(&key), landed);
        TreeRow {
            entry: TreeEntry {
                name: String::new(),
                path: dir.to_path_buf(),
                is_dir: true,
                ignored: false,
            },
            depth,
            is_root: false,
            expanded: false,
            note: Some(note),
        }
    }

    fn invalidate_dir(&mut self, host: HostId, dir: &Path) -> bool {
        let key: DirKey = (host, dir.to_path_buf());
        let cached = self.children.get(host, dir).is_some();
        if cached {
            self.stale.insert(key.clone());
        }
        let pending = self.loads.is_pending(&key);
        self.loads.invalidate(&key);
        cached || pending
    }

    fn gitignore_reaches_tree(&self, host: HostId, paths: &HashSet<PathBuf>) -> bool {
        paths
            .iter()
            .filter(|p| p.file_name().is_some_and(|n| n == ".gitignore"))
            .filter_map(|p| p.parent())
            .any(|dir| {
                self.children
                    .keys()
                    .any(|(id, cached)| id == host && cached.starts_with(dir))
                    || self
                        .loads
                        .pending_keys()
                        .any(|(id, pending)| *id == host && pending.starts_with(dir))
            })
    }

    fn invalidate_all(&mut self) {
        self.stale
            .extend(self.children.keys().map(|(host, dir)| (host, dir.clone())));
        self.loads.invalidate_all();
        self.search.restart();
    }

    fn invalidate_repo_roots(&mut self) -> bool {
        let had = !self.repo_roots.is_empty() || !self.repo_root_loads.is_empty();
        self.repo_roots.clear();
        self.repo_root_loads.invalidate_all();
        had
    }

    fn optimistic(
        &mut self,
        host: HostId,
        dir: &Path,
        op: &TreeWrite,
        target: &TreeEntry,
    ) -> Option<Vec<TreeEntry>> {
        self.loads.invalidate(&(host, dir.to_path_buf()));
        optimistic_write(&mut self.children, host, dir, op, target)
    }

    fn rollback(&mut self, host: HostId, dir: &Path, before: Option<Vec<TreeEntry>>) {
        rollback_write(&mut self.children, host, dir, before)
    }
}

pub(crate) fn sort_entries(entries: &mut [TreeEntry]) {
    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
}

fn optimistic_write(
    children: &mut ByHost<PathBuf, Vec<TreeEntry>>,
    host: HostId,
    dir: &Path,
    op: &TreeWrite,
    target: &TreeEntry,
) -> Option<Vec<TreeEntry>> {
    let key = dir.to_path_buf();
    let before = children.get(host, &key).cloned();
    if let Some(mut entries) = children.remove(host, &key) {
        match op {
            TreeWrite::Rename { from } => entries.retain(|e| e.path != *from),
            TreeWrite::Delete => entries.retain(|e| e.path != target.path),
            TreeWrite::NewFile | TreeWrite::NewFolder => {}
        }
        if !matches!(op, TreeWrite::Delete) {
            entries.push(target.clone());
            sort_entries(&mut entries);
        }
        children.insert(host, key, entries);
    }
    before
}

fn rollback_write(
    children: &mut ByHost<PathBuf, Vec<TreeEntry>>,
    host: HostId,
    dir: &Path,
    before: Option<Vec<TreeEntry>>,
) {
    drop(before);
    children.remove(host, &dir.to_path_buf());
}

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

impl Tty7App {
    pub(crate) fn active_host(&self, cx: &App) -> Option<SharedHost> {
        HostRegistry::lookup(cx, self.spawn_host(cx))
    }

    pub(crate) fn file_tree_refresh_roots(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let id = self.spawn_host(cx);
        let Some(host) = self.active_host(cx) else {
            return;
        };
        let leaves = match self.tabs.get(self.active) {
            Some(tab) => tab.pane.terminals(),
            None => Vec::new(),
        };
        // `effective_cwd`, not `cwd`: a pane running an agent that moved into
        // a git worktree keeps its kernel cwd back at the launch directory, so
        // the raw process cwd would root the tree in the wrong checkout — and
        // in the wrong one *visibly*, since the cwd row directly above this
        // tree already follows the agent.
        let cwds: Vec<PathBuf> = leaves
            .iter()
            .filter(|leaf| leaf.read(cx).host_id() == id)
            .filter_map(|leaf| leaf.read(cx).effective_cwd())
            .collect();
        let mut roots: Vec<PathBuf> = Vec::new();
        let mut resolved = true;
        for cwd in &cwds {
            match self.file_tree.repo_roots.get(id, cwd) {
                Some(root) => {
                    if !roots.contains(root) {
                        roots.push(root.clone());
                    }
                }
                None => {
                    resolved = false;
                    self.file_tree_request_repo_root(&host, cwd.clone(), cx);
                }
            }
        }
        if !resolved {
            return;
        }
        if roots.is_empty()
            && id.is_local()
            && let Some(home) = std::env::var_os("HOME")
        {
            roots.push(PathBuf::from(home));
        }
        let _ = window;
        let Some(code) = self.tab_code_mut_or_init() else {
            return;
        };
        if roots != code.roots {
            code.roots = roots;
            self.file_tree.invalidate_all();
            cx.notify();
        }
        self.file_tree_sync_watch(host, cx);
    }

    fn file_tree_sync_watch(&mut self, host: SharedHost, cx: &mut Context<Self>) {
        let union: HashSet<PathBuf> = self
            .tabs
            .iter()
            .filter_map(|t| t.code.as_deref())
            .flat_map(|c| c.roots.iter().chain(c.expanded.iter()).cloned())
            .collect();
        if union != self.file_tree.watched {
            self.file_tree.sync_watch(host, union, cx);
        }
    }

    fn file_tree_request_repo_root(
        &mut self,
        host: &SharedHost,
        cwd: PathBuf,
        cx: &mut Context<Self>,
    ) {
        let id = host.id();
        let key: DirKey = (id, cwd.clone());
        if !self.file_tree.repo_root_loads.begin(key.clone()) {
            return;
        }
        HostOps::run(
            host.clone(),
            cx,
            {
                let cwd = cwd.clone();
                move |h| h.repo_root(&cwd).ok().flatten()
            },
            move |app, root, cx| {
                if app.file_tree.repo_root_loads.finish(&key) {
                    app.file_tree
                        .repo_roots
                        .insert(id, cwd.clone(), root.unwrap_or(cwd));
                }
                cx.notify();
            },
        );
    }

    pub(crate) fn file_tree_on_screen(&self, cx: &App) -> bool {
        self.right_panel_open(cx)
            && self.right_panel_tab == RightPanelTab::Files
            && self.sftp_panel.open_pane_id.is_none()
    }

    fn file_tree_query(&self, cx: &App) -> String {
        self.file_search.read(cx).value().trim().to_lowercase()
    }

    pub(crate) fn file_tree_searching(&self, cx: &App) -> bool {
        !self.file_tree_query(cx).is_empty()
    }

    pub(crate) fn file_tree_listings_on_screen(&self, cx: &App) -> bool {
        self.file_tree_on_screen(cx) && !self.file_tree_searching(cx)
    }

    pub(crate) fn file_tree_apply_fs_events(
        &mut self,
        host: HostId,
        paths: &HashSet<PathBuf>,
        cx: &mut Context<Self>,
    ) {
        log::debug!(
            target: "tty7::file_tree",
            "fs events on host {host:?}: {:?}",
            paths.iter().take(8).collect::<Vec<_>>()
        );
        let on_screen = self.file_tree_on_screen(cx);
        let listings_on_screen = self.file_tree_listings_on_screen(cx);
        let mut roots_moved = false;
        if paths.iter().any(|p| {
            p.file_name().is_some_and(|n| n == ".git")
                || p.parent()
                    .and_then(Path::file_name)
                    .is_some_and(|n| n == ".git")
        }) {
            roots_moved = self.file_tree.invalidate_repo_roots();
        }
        // Working-tree edits the source control cache has no other way to hear
        // about. Anything under `.git` is skipped: the repository has its own
        // watch, and routing it through here would only double the events that
        // land in one debounce window.
        let mut announced: HashSet<&Path> = HashSet::new();
        for path in paths {
            if path.components().any(|c| c.as_os_str() == ".git") {
                continue;
            }
            let Some(dir) = path.parent() else { continue };
            if announced.insert(dir) {
                self.scm_invalidate_cwd(host, dir, cx);
            }
        }

        let gitignore_touched = paths
            .iter()
            .any(|p| p.file_name().is_some_and(|n| n == ".gitignore"));
        if gitignore_touched && self.file_tree.gitignore_reaches_tree(host, paths) {
            self.file_tree.invalidate_all();
            if on_screen {
                cx.notify();
            }
        } else {
            let mut touched = false;
            for dir in dirs_to_relist(paths, self.file_tree.show_hidden) {
                touched |= self.file_tree.invalidate_dir(host, dir);
            }
            if roots_moved && on_screen {
                cx.notify();
            }
            if !touched {
                return;
            }

            if !listings_on_screen {
                return;
            }
            let Some(shared) = self.active_host(cx) else {
                return;
            };
            if shared.id() != host {
                return;
            }
            let (roots, expanded) = match self.tab_code() {
                Some(code) => (code.roots.clone(), code.expanded.clone()),
                None => return,
            };
            self.file_tree.request_loads(&shared, &roots, &expanded, cx);
        }
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

    fn file_tree_key_down(
        &mut self,
        ev: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let host = self.spawn_host(cx);
        let Some(code) = self.tab_code() else {
            return;
        };
        // Placeholder rows explain an absence; they are not files, so the
        // cursor must not be able to land on one.
        let rows: Vec<TreeRow> = self
            .file_tree
            .visible_rows(host, &code.roots, &code.expanded)
            .into_iter()
            .filter(|r| r.note.is_none())
            .collect();
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

    fn file_tree_begin_edit(
        &mut self,
        edit_for: TreeEditKind,
        target: &Path,
        target_is_dir: bool,
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
                TreeEditKind::NewFile => t(L10nKey::FileTreePlaceholderFileName),
                TreeEditKind::NewFolder => t(L10nKey::FileTreePlaceholderFolderName),
                TreeEditKind::Rename => t(L10nKey::FileTreePlaceholderNewName),
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
        let host_dir = if target_is_dir {
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
        let Some(host) = self.active_host(cx) else {
            return;
        };
        let id = host.id();
        let dir = edit.host_dir().to_path_buf();
        let (new_path, is_dir, op): (PathBuf, bool, TreeWrite) = match &edit {
            TreeEdit::NewFile { dir, .. } => (host.join(dir, &name), false, TreeWrite::NewFile),
            TreeEdit::NewFolder { dir, .. } => (host.join(dir, &name), true, TreeWrite::NewFolder),
            TreeEdit::Rename { path, .. } => {
                let was_dir = self
                    .file_tree
                    .children
                    .get(id, &dir)
                    .and_then(|entries| entries.iter().find(|e| e.path == *path))
                    .is_some_and(|e| e.is_dir);
                let parent = path.parent().unwrap_or(path);
                (
                    host.join(parent, &name),
                    was_dir,
                    TreeWrite::Rename { from: path.clone() },
                )
            }
        };

        let row = TreeEntry {
            name: name.clone(),
            path: new_path.clone(),
            is_dir,
            ignored: false,
        };
        let rollback = self.file_tree.optimistic(id, &dir, &op, &row);
        if let Some(code) = self.tab_code_mut() {
            code.selected = Some(new_path.clone());
        }

        let target = new_path.clone();
        // The same gap `file_tree_delete` closed: on its own, "Permission
        // denied (os error 13)" says neither which file nor what was being
        // done to it, and those are the only two things worth knowing here.
        // A rename is named by the name it is leaving, which is the one still
        // on screen to find.
        let (context, failed_name) = match &edit {
            TreeEdit::Rename { path, .. } => (
                L10nKey::FileTreeRenameFailed,
                path.file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| name.clone()),
            ),
            _ => (L10nKey::FileTreeCreateFailed, name.clone()),
        };
        HostOps::run_in(
            host,
            window,
            cx,
            move |h| match &op {
                TreeWrite::NewFile => h.create_file_new(&target),
                TreeWrite::NewFolder => h.create_dir(&target, false),
                TreeWrite::Rename { from } => h.rename(from, &target),
                TreeWrite::Delete => h.remove(&target, is_dir),
            },
            move |app, result: std::io::Result<()>, window, cx| {
                match result {
                    Ok(()) => {
                        app.file_tree.invalidate_dir(id, &dir);
                        if matches!(edit, TreeEdit::NewFile { .. }) {
                            app.open_file_in_editor(&new_path, window, cx);
                        }
                    }
                    Err(e) => {
                        app.file_tree.rollback(id, &dir, rollback);
                        if let Some(code) = app.tab_code_mut()
                            && code.selected.as_deref() == Some(&*new_path)
                        {
                            code.selected = None;
                        }
                        HostOps::notify_err(
                            window,
                            cx,
                            &t_fmt(context, &[("name", &failed_name)]),
                            &e,
                        );
                    }
                }
                cx.notify();
            },
        );
        cx.notify();
    }

    fn file_tree_delete(
        &mut self,
        path: PathBuf,
        is_dir: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| path.display().to_string());
        let detail = if is_dir {
            t(L10nKey::FileTreeDeleteFolderBody)
        } else {
            t(L10nKey::FileTreeDeleteFileBody)
        };
        let answer = window.prompt(
            PromptLevel::Warning,
            &t_fmt(L10nKey::FileTreeDeleteTitle, &[("name", &name)]),
            Some(detail),
            &crate::ui::confirm_answers(t(L10nKey::Delete), t(L10nKey::Cancel)),
            cx,
        );
        cx.spawn_in(window, async move |app, cx| {
            let Ok(0) = answer.await else { return };
            let _ = app.update_in(cx, |app, window, cx| {
                let Some(host) = app.active_host(cx) else {
                    return;
                };
                let id = host.id();
                let Some(parent) = path.parent().map(Path::to_path_buf) else {
                    return;
                };
                let row = TreeEntry {
                    name: name.clone(),
                    path: path.clone(),
                    is_dir,
                    ignored: false,
                };
                let rollback = app
                    .file_tree
                    .optimistic(id, &parent, &TreeWrite::Delete, &row);
                if let Some(code) = app.tab_code_mut()
                    && code.selected.as_deref() == Some(&path)
                {
                    code.selected = None;
                }
                let target = path.clone();
                // "Delete failed: Permission denied" leaves out the one thing
                // you need in a tree of files, the same way "Save failed" used
                // to in the editor.
                let failed_name = name.clone();
                HostOps::run_in(
                    host,
                    window,
                    cx,
                    move |h| h.remove(&target, is_dir),
                    move |app, result: std::io::Result<()>, window, cx| {
                        match result {
                            Ok(()) => {
                                app.file_tree.invalidate_dir(id, &parent);
                            }
                            Err(e) => {
                                app.file_tree.rollback(id, &parent, rollback);
                                HostOps::notify_err(
                                    window,
                                    cx,
                                    &t_fmt(
                                        L10nKey::FileTreeDeleteFailed,
                                        &[("name", &failed_name)],
                                    ),
                                    &e,
                                );
                            }
                        }
                        cx.notify();
                    },
                );
                cx.notify();
            });
        })
        .detach();
    }

    /// Copy what was dropped on the tree into `dir`.
    ///
    /// The drop is the whole gesture: the panel does not ask where to put the
    /// files, it puts them where the cursor was. The one question it does ask
    /// is about replacing something already there, and that question is asked
    /// before anything has been written.
    fn file_tree_drop_paths(
        &mut self,
        sources: Vec<PathBuf>,
        dir: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.file_tree_copy_into(sources, dir, false, window, cx);
    }

    fn file_tree_copy_into(
        &mut self,
        sources: Vec<PathBuf>,
        dir: PathBuf,
        overwrite: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if sources.is_empty() {
            return;
        }
        let Some(host) = self.active_host(cx) else {
            return;
        };
        let id = host.id();
        let asked_for = sources.clone();
        let target = dir.clone();
        HostOps::run_in(
            host,
            window,
            cx,
            move |h| file_copy::copy_into_dir(h, &sources, &target, overwrite),
            move |app, report: file_copy::DropReport, window, cx| {
                if !report.copied.is_empty() {
                    app.file_tree.invalidate_dir(id, &dir);
                }
                if !report.conflicts.is_empty() {
                    app.file_tree_confirm_replace(asked_for, dir, report.conflicts, window, cx);
                } else if let Some((name, e)) = report.errors.first() {
                    // One notification for the drop, not one per file: a folder
                    // of unreadable files would otherwise bury the screen.
                    let context = match report.errors.len() {
                        1 => t_fmt(L10nKey::FileDropFailed, &[("name", name)]),
                        n => t_fmt(
                            L10nKey::FileDropFailedMany,
                            &[("name", name), ("n", &(n - 1).to_string())],
                        ),
                    };
                    HostOps::notify_err(window, cx, &context, e);
                }
                cx.notify();
            },
        );
    }

    fn file_tree_confirm_replace(
        &mut self,
        sources: Vec<PathBuf>,
        dir: PathBuf,
        conflicts: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let title = match conflicts.as_slice() {
            [one] => t_fmt(L10nKey::FileDropReplaceTitle, &[("name", one)]),
            many => t_fmt(
                L10nKey::FileDropReplaceManyTitle,
                &[("n", &many.len().to_string())],
            ),
        };
        let answer = window.prompt(
            PromptLevel::Warning,
            &title,
            Some(t(L10nKey::FileDropReplaceBody)),
            &crate::ui::confirm_answers(t(L10nKey::FileDropReplace), t(L10nKey::Cancel)),
            cx,
        );
        cx.spawn_in(window, async move |app, cx| {
            let Ok(0) = answer.await else { return };
            let _ = app.update_in(cx, |app, window, cx| {
                app.file_tree_copy_into(sources, dir, true, window, cx);
            });
        })
        .detach();
    }

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

    fn file_tree_attach_to_agent(&mut self, path: &Path, cx: &mut Context<Self>) {
        let Some(target) = self.agent_target_leaf(cx) else {
            crate::terminal::notify_desktop(Some("tty7"), t(L10nKey::AppNoRunningCodingAgent));
            return;
        };
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

#[derive(Clone, Copy)]
enum TreeEditKind {
    NewFile,
    NewFolder,
    Rename,
}

enum TreeWrite {
    NewFile,
    NewFolder,
    Rename { from: PathBuf },
    Delete,
}

impl Tty7App {
    pub(crate) fn render_file_tree_rows(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        self.file_tree_refresh_roots(window, cx);
        let (roots, expanded) = match self.tab_code() {
            Some(code) => (code.roots.clone(), code.expanded.clone()),
            None => (Vec::new(), std::collections::HashSet::new()),
        };
        let query = self.file_tree_query(cx);
        let host = self.active_host(cx);
        let host_id = self.spawn_host(cx);
        if let Some(host) = host.clone() {
            self.file_tree_sync_watch(host, cx);
        }
        let decor = self.file_tree_decorations(host.as_ref(), host_id, &roots, cx);
        self.file_tree.sync_search(&query, &roots, cx);
        let rows = if self.file_tree_searching(cx) {
            self.file_tree.search_rows()
        } else {
            if let Some(host) = &host {
                self.file_tree.request_loads(host, &roots, &expanded, cx);
            }
            self.file_tree.visible_rows(host_id, &roots, &expanded)
        };
        // A search that found nothing, and a tab with no directory behind it,
        // both used to render as an empty column that looks identical to a
        // tree still loading.
        let blank = rows.is_empty().then(|| {
            let text = match self.file_tree_searching(cx) {
                true => t_fmt(L10nKey::SettingsNothingMatches, &[("query", &query)]),
                false => t(L10nKey::OpenFileFromTree).to_string(),
            };
            div()
                .px_3()
                .py_4()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child(text)
        });
        let column = v_flex()
            .id("right-panel-tree-rows")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .track_scroll(&self.right_panel.tree_scroll)
            .px_1()
            .pb_1()
            .track_focus(&self.file_tree.focus_handle)
            .on_key_down(cx.listener(|this, ev: &KeyDownEvent, window, cx| {
                this.file_tree_key_down(ev, window, cx);
            }))
            .children(blank)
            .children(rows.iter().flat_map(|row| {
                let deco = row_decoration(&decor, &row.entry);
                self.render_tree_row(row, deco, window, cx)
            }))
            // Everything the rows do not cover — the gap below the last one,
            // and the whole column while the tree is still empty — belongs to
            // the top of the tree. A row under the cursor wins: gpui hands a
            // drop to the innermost target first, and it stops there.
            .when_some(roots.first().cloned(), |d, root| {
                d.drag_over::<ExternalPaths>(|s, _, _, cx| {
                    s.bg(cx.theme().drag_border.opacity(0.06))
                })
                .on_drop(cx.listener(
                    move |this, paths: &ExternalPaths, window, cx| {
                        this.file_tree_drop_paths(paths.paths().to_vec(), root.clone(), window, cx);
                    },
                ))
            });
        crate::ui::scrollbar::with_vertical_scrollbar(
            "right-panel-tree-scrollbar",
            column,
            &self.right_panel.tree_scroll,
        )
    }

    /// Ask each root for a fresh status and take the index it already holds.
    ///
    /// The `Arc` is cloned here, outside the row loop: a tree can be thousands
    /// of rows and every one of them wants the same index. `scm_refresh` is
    /// idempotent and drops a probe that is already running or already current,
    /// which is what makes it safe from a render.
    fn file_tree_decorations(
        &mut self,
        host: Option<&SharedHost>,
        host_id: HostId,
        roots: &[PathBuf],
        cx: &mut Context<Self>,
    ) -> Decorations {
        let mut decor: Decorations = Vec::new();
        for root in roots {
            if let Some(host) = host {
                self.scm_refresh(host.clone(), root.clone(), cx);
            }
            if let Some(index) = index_of(cx, host_id, root) {
                decor.push((root.clone(), index));
            }
        }
        order_innermost_first(&mut decor);
        decor
    }

    fn render_tree_row(
        &self,
        row: &TreeRow,
        deco: RowDeco,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let path = row.entry.path.clone();
        let is_dir = row.entry.is_dir;
        let selected = self.tab_code().and_then(|c| c.selected.as_deref()) == Some(&*path);
        let muted = cx.theme().muted_foreground;

        // A placeholder standing in for children that are not there. Not a
        // file, so it takes none of the row machinery below — no hover, no
        // selection, no context menu, no drag. It does take a drop: it is
        // drawn inside a folder and it is the only thing in an empty one, so
        // letting it fall through to the root would put files somewhere the
        // cursor never was.
        if let Some(note) = row.note {
            let (key, ink) = match note {
                TreeNote::Loading => (L10nKey::TreeDirLoading, muted),
                TreeNote::Empty => (L10nKey::TreeDirEmpty, muted),
                TreeNote::HiddenOnly => (L10nKey::TreeDirHiddenOnly, muted),
                TreeNote::Unreadable => (L10nKey::TreeDirUnreadable, cx.theme().danger),
                TreeNote::SearchCapped => (L10nKey::TreeSearchCapped, muted),
                TreeNote::SearchFailed => (L10nKey::TreeSearchFailed, cx.theme().danger),
            };
            return vec![
                h_flex()
                    // Aligned with the label column of a real row at this
                    // depth: 6 for the row's own inset, INDENT for the depth,
                    // then the width of the icon and its gap.
                    .pl(px(6.0 + row.depth as f32 * INDENT + 20.0))
                    .py_1()
                    .items_center()
                    .text_xs()
                    .italic()
                    .text_color(ink)
                    .child(match note {
                        TreeNote::SearchCapped => t_fmt(key, &[("n", &SEARCH_LIMIT.to_string())]),
                        _ => t(key).to_string(),
                    })
                    // Every note but the two search ones stands for a real
                    // directory, and carries its path; those stand for the rest
                    // of a search, or for one that never ran, and have nowhere
                    // to put anything.
                    .when(!path.as_os_str().is_empty(), |d| {
                        d.drag_over::<ExternalPaths>(|s, _, _, cx| {
                            s.bg(cx.theme().drag_border.opacity(0.14))
                        })
                        .on_drop(cx.listener(
                            move |this, paths: &ExternalPaths, window, cx| {
                                this.file_tree_drop_paths(
                                    paths.paths().to_vec(),
                                    path.clone(),
                                    window,
                                    cx,
                                );
                            },
                        ))
                    })
                    .into_any_element(),
            ];
        }

        let sf = cx.global::<crate::ui::presets::Surfaces>().popover;
        let dirty = self
            .tab_code()
            .is_some_and(|c| c.files.iter().any(|f| f.dirty && f.path == *path));

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
                // The name carrying the colour is the signal people actually
                // read; the letter at the end of the row is the confirmation.
                .when_some(deco.tint, |d, status| {
                    d.text_color(status_color(status, cx))
                })
                .when(deco.strike, |d| d.line_through())
                .when(deco.bold, |d| d.font_weight(gpui::FontWeight::SEMIBOLD))
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
            .when(selected, |d| d.bg(gpui::rgb(sf.selected)))
            .when(!selected, |d| d.hover(|s| s.bg(gpui::rgb(sf.hover))))
            .child(Icon::new(icon).xsmall().text_color(if is_dir {
                cx.theme().foreground
            } else {
                muted
            }))
            .child(label)
            // Two indicators, two columns, two shapes. The dot is an unsaved
            // editor buffer and has nothing to do with git; keeping it round and
            // `warning` while the git letter sits in its own trailing cell is
            // what stops the two from ever being read as one.
            .when(dirty, |d| {
                d.child(
                    div()
                        .flex_none()
                        .size(px(6.))
                        .rounded_full()
                        .bg(cx.theme().warning),
                )
            })
            .when_some(deco.badge(), |d, (letter, status)| {
                d.child(git_badge(
                    letter,
                    status_color(status, cx),
                    &cx.theme().mono_font_family,
                ))
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
            .on_drag(ExternalPaths(vec![path.clone()].into()), {
                let name = row.entry.name.clone();
                move |_, _, _, cx| {
                    let name = name.clone();
                    cx.new(|_| DragGhost { name })
                }
            })
            // The other direction: files dropped on this row are copied in.
            // A folder takes them itself; a file stands in for the folder it
            // is in, which is where "put it next to this one" lands.
            .drag_over::<ExternalPaths>(|s, _, _, cx| s.bg(cx.theme().drag_border.opacity(0.14)))
            .on_drop(cx.listener({
                let dir = match is_dir {
                    true => path.clone(),
                    false => path.parent().unwrap_or(&path).to_path_buf(),
                };
                move |this, paths: &ExternalPaths, window, cx| {
                    this.file_tree_drop_paths(paths.paths().to_vec(), dir.clone(), window, cx);
                }
            }))
            .context_menu({
                let app = cx.entity().downgrade();
                let path = path.clone();
                let is_root = row.is_root;
                let show_hidden = self.file_tree.show_hidden;
                let paths_are_local = self.spawn_host(cx).is_local();
                move |menu, _window, cx| {
                    let danger = cx.theme().danger;
                    Self::tree_row_context_menu(
                        menu,
                        &path,
                        is_dir,
                        is_root,
                        show_hidden,
                        paths_are_local,
                        danger,
                        &app,
                    )
                }
            });

        let mut out: Vec<AnyElement> = vec![row_el.into_any_element()];

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

    fn tree_row_context_menu(
        menu: PopupMenu,
        path: &Path,
        is_dir: bool,
        is_root: bool,
        show_hidden: bool,
        // The tree lists whatever host the workspace spawns on. Everything else
        // in this menu goes through that host; the file manager only knows this
        // machine, so over SSH the item would hand Finder a path that is not
        // here — silently opening nothing, or the wrong thing if a local path
        // happens to collide.
        paths_are_local: bool,
        danger: gpui::Hsla,
        app: &gpui::WeakEntity<Self>,
    ) -> PopupMenu {
        let mut menu = menu.min_w(px(200.));
        let p = path.to_path_buf();

        if !is_dir {
            menu = menu.item(
                PopupMenuItem::new(t(L10nKey::FileTreeContextOpen)).on_click({
                    let app = app.clone();
                    let p = p.clone();
                    move |_, window, cx| {
                        let _ = app.update(cx, |this, cx| this.open_file_in_editor(&p, window, cx));
                    }
                }),
            );
        }
        if is_dir {
            menu = menu.item(
                PopupMenuItem::new(t(L10nKey::FileTreeContextCdHere)).on_click({
                    let app = app.clone();
                    let p = p.clone();
                    move |_, window, cx| {
                        let _ = app.update(cx, |this, cx| this.file_tree_cd(&p, window, cx));
                    }
                }),
            );
        }
        menu = menu
            .item(
                PopupMenuItem::new(t(L10nKey::FileTreeContextInsertPath)).on_click({
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
                }),
            )
            .item(
                PopupMenuItem::new(t(L10nKey::FileTreeContextAttachAgent)).on_click({
                    let app = app.clone();
                    let p = p.clone();
                    move |_, _window, cx| {
                        let _ = app.update(cx, |this, cx| this.file_tree_attach_to_agent(&p, cx));
                    }
                }),
            )
            .separator()
            .item(
                PopupMenuItem::new(t(L10nKey::FileTreeContextNewFile)).on_click({
                    let app = app.clone();
                    let p = p.clone();
                    move |_, window, cx| {
                        let _ = app.update(cx, |this, cx| {
                            this.file_tree_begin_edit(TreeEditKind::NewFile, &p, is_dir, window, cx)
                        });
                    }
                }),
            )
            .item(
                PopupMenuItem::new(t(L10nKey::FileTreeContextNewFolder)).on_click({
                    let app = app.clone();
                    let p = p.clone();
                    move |_, window, cx| {
                        let _ = app.update(cx, |this, cx| {
                            this.file_tree_begin_edit(
                                TreeEditKind::NewFolder,
                                &p,
                                is_dir,
                                window,
                                cx,
                            )
                        });
                    }
                }),
            );

        if !is_root {
            menu = menu.item(
                PopupMenuItem::new(t(L10nKey::FileTreeContextRename)).on_click({
                    let app = app.clone();
                    let p = p.clone();
                    move |_, window, cx| {
                        let _ = app.update(cx, |this, cx| {
                            this.file_tree_begin_edit(TreeEditKind::Rename, &p, is_dir, window, cx)
                        });
                    }
                }),
            );
        }

        menu = menu.separator().item(
            PopupMenuItem::new(t(L10nKey::FileTreeContextCopyPath)).on_click({
                let p = p.clone();
                move |_, _window, cx| {
                    cx.write_to_clipboard(gpui::ClipboardItem::new_string(p.display().to_string()));
                }
            }),
        );
        if paths_are_local {
            menu = menu.item(
                PopupMenuItem::new(crate::ui::right_panel::reveal_label()).on_click({
                    let p = p.clone();
                    move |_, _window, cx| {
                        cx.reveal_path(&p);
                    }
                }),
            );
        }

        menu = menu.separator().item(dotfiles_menu_item(show_hidden, app));

        if !is_root {
            menu = menu.separator().item(
                PopupMenuItem::element(move |_window, _cx| {
                    div().text_color(danger).child(t(L10nKey::Delete))
                })
                .on_click({
                    let app = app.clone();
                    move |_, window, cx| {
                        let p = p.clone();
                        let _ =
                            app.update(cx, |this, cx| this.file_tree_delete(p, is_dir, window, cx));
                    }
                }),
            );
        }
        menu
    }
}

fn dotfiles_menu_item(show_hidden: bool, app: &gpui::WeakEntity<Tty7App>) -> PopupMenuItem {
    let app = app.clone();
    PopupMenuItem::new(if show_hidden {
        t(L10nKey::FileTreeContextHideDotfiles)
    } else {
        t(L10nKey::FileTreeContextShowDotfiles)
    })
    .on_click(move |_, _window, cx| {
        let _ = app.update(cx, |this, cx| {
            this.file_tree.show_hidden = !this.file_tree.show_hidden;
            cx.notify();
        });
    })
}

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

fn land_listing(
    loads: &mut InFlight<DirKey>,
    children: &mut ByHost<PathBuf, Vec<TreeEntry>>,
    stale: &mut HashSet<DirKey>,
    key: &DirKey,
    id: HostId,
    dir: PathBuf,
    entries: Vec<TreeEntry>,
) -> bool {
    let superseded = !loads.finish(key);
    children.insert(id, dir, entries);
    stale.remove(key);
    superseded
}

fn dirs_to_relist(paths: &HashSet<PathBuf>, show_hidden: bool) -> HashSet<&Path> {
    paths
        .iter()
        .filter(|p| event_can_change_a_row(p, show_hidden))
        .filter_map(|p| p.parent())
        .collect()
}

/// Every repository behind the tree, paired with the root its index keys are
/// relative to. Built once per render and ordered innermost-first.
type Decorations = Vec<(PathBuf, Arc<StatusIndex>)>;

/// What git says about one row: a letter for the trailing badge and the shape
/// of the name beside it.
///
/// The colour is carried as a `DecoStatus` rather than an `Hsla` so that it
/// resolves through the one table in `scm::status` — the panel, the diff cards
/// and the tree cannot grow three opinions about what "modified" looks like —
/// and so that everything below stays a pure function.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
struct RowDeco {
    /// Empty wherever no badge is drawn: directories, because a folder is not
    /// "M", and ignored rows, because a tree full of `!` is noise.
    letter: &'static str,
    tint: Option<DecoStatus>,
    strike: bool,
    bold: bool,
}

impl RowDeco {
    fn file(status: DecoStatus) -> RowDeco {
        RowDeco {
            letter: status_glyph(status),
            tint: Some(status),
            // The name says "gone" twice — struck through and greyed — because
            // the row still occupies a slot in a listing that no longer has the
            // file in it.
            strike: status == DecoStatus::Deleted,
            bold: status == DecoStatus::Conflict,
        }
    }

    /// A directory is two states, never seven: work happened under it, or a
    /// conflict is waiting under it. `Modified` and `Conflict` appear here only
    /// as the way to reach `warning` and `danger` through the shared table.
    fn dir(rollup: DirRollup) -> RowDeco {
        let tint = if rollup.conflict {
            Some(DecoStatus::Conflict)
        } else if rollup.changed {
            Some(DecoStatus::Modified)
        } else {
            None
        };
        RowDeco {
            tint,
            ..RowDeco::default()
        }
    }

    /// The badge is laid out only where there is a letter for it, so a tree with
    /// no repository behind it gives up no width.
    fn badge(&self) -> Option<(&'static str, DecoStatus)> {
        let status = self.tint?;
        (!self.letter.is_empty()).then_some((self.letter, status))
    }
}

/// `StatusIndex` is keyed by a repo-root-relative, `/`-separated path. Borrowed
/// rather than built, which on Unix is every row.
fn repo_relative<'a>(root: &Path, path: &'a Path) -> Option<Cow<'a, str>> {
    let rel = path.strip_prefix(root).ok()?.to_str()?;
    if rel.is_empty() {
        // The root row itself, which has no key and nothing worth saying:
        // "this repository contains changes" is not news.
        return None;
    }
    Some(with_forward_slashes(rel, std::path::MAIN_SEPARATOR))
}

/// Split out of `repo_relative` so the Windows separator is reachable from a
/// test on any platform — `strip_prefix` only ever splits on the host's own
/// separator, which leaves a backslash path untestable through the caller.
fn with_forward_slashes(text: &str, sep: char) -> Cow<'_, str> {
    if sep == '/' || !text.contains(sep) {
        return Cow::Borrowed(text);
    }
    Cow::Owned(text.replace(sep, "/"))
}

/// Innermost first, so a submodule nested inside another root answers for its
/// own files instead of the repository that contains it.
fn order_innermost_first(decor: &mut Decorations) {
    decor.sort_by_key(|(root, _)| std::cmp::Reverse(root.as_os_str().len()));
}

/// One hash probe per row and no allocation on the path that matters.
fn row_decoration(decor: &Decorations, entry: &TreeEntry) -> RowDeco {
    // A gitignored row keeps the italic-and-dim it has always worn and takes
    // nothing else: a letter and a colour would be describing a file the
    // repository is not tracking.
    if entry.ignored {
        return RowDeco::default();
    }
    for (root, index) in decor {
        let Some(rel) = repo_relative(root, &entry.path) else {
            continue;
        };
        return if entry.is_dir {
            index.dir(&rel).map(RowDeco::dir).unwrap_or_default()
        } else {
            // `file` comes back empty once the change count blew past
            // `MAX_DECORATED_FILES`. The rollups survive that, so the folders
            // keep saying where the work is.
            index.file(&rel).map(RowDeco::file).unwrap_or_default()
        };
    }
    RowDeco::default()
}

fn event_can_change_a_row(path: &Path, show_hidden: bool) -> bool {
    show_hidden
        || !path
            .file_name()
            .is_some_and(|n| n.to_string_lossy().starts_with('.'))
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
    fn an_expanded_directory_says_why_it_has_no_children() {
        // Still in flight.
        assert_eq!(dir_note(false, None), TreeNote::Loading);
        // A listing that came back with nothing in it.
        assert_eq!(dir_note(false, Some(0)), TreeNote::Empty);
        // Entries landed, but the hidden filter took every one of them.
        assert_eq!(dir_note(false, Some(3)), TreeNote::HiddenOnly);
        // A directory the OS refused. `read_dir` used to be
        // `unwrap_or_default`ed, so this was byte-identical to Empty.
        assert_eq!(dir_note(true, Some(0)), TreeNote::Unreadable);
        assert_eq!(
            dir_note(true, None),
            TreeNote::Unreadable,
            "a known failure outranks a retry still in flight"
        );
    }

    #[test]
    fn a_failed_search_says_so_instead_of_drawing_no_rows() {
        let mut search = SearchState::default();
        let walk = search.retarget("foo", false).expect("a new query walks");
        search.accept(walk, false, Vec::new());
        assert_eq!(
            search_rows(&search)
                .iter()
                .filter_map(|r| r.note)
                .collect::<Vec<_>>(),
            vec![TreeNote::SearchFailed],
            "without a note the column falls through to \"Nothing matches\""
        );

        // A search that ran and found nothing still draws nothing.
        let walk = search
            .retarget("bar", false)
            .expect("a changed query walks");
        search.accept(walk, true, Vec::new());
        assert!(search_rows(&search).is_empty());
    }

    #[test]
    fn a_listing_superseded_in_flight_is_still_shown() {
        let mut loads: InFlight<DirKey> = InFlight::default();
        let mut children: ByHost<PathBuf, Vec<TreeEntry>> = ByHost::default();
        let id = HostId::LOCAL;
        let dir = PathBuf::from("/home/me");
        let key: DirKey = (id, dir.clone());

        let mut stale: HashSet<DirKey> = HashSet::new();

        assert!(loads.begin(key.clone()), "the listing goes out");
        loads.invalidate(&key);

        let again = land_listing(
            &mut loads,
            &mut children,
            &mut stale,
            &key,
            id,
            dir.clone(),
            vec![entry("src", true)],
        );
        assert!(again, "superseded, so the caller goes round again");
        assert!(
            children.get(id, &dir).is_some(),
            "the snapshot is on screen rather than thrown away"
        );

        assert!(loads.begin(key.clone()));
        let again = land_listing(
            &mut loads,
            &mut children,
            &mut stale,
            &key,
            id,
            dir.clone(),
            vec![entry("src", true)],
        );
        assert!(!again, "nothing superseded it, so one listing is enough");
    }

    #[test]
    fn an_outdated_listing_stays_on_screen_until_its_replacement_lands() {
        let mut loads: InFlight<DirKey> = InFlight::default();
        let mut children: ByHost<PathBuf, Vec<TreeEntry>> = ByHost::default();
        let mut stale: HashSet<DirKey> = HashSet::new();
        let id = HostId::LOCAL;
        let dir = PathBuf::from("/home/me");
        let key: DirKey = (id, dir.clone());

        loads.begin(key.clone());
        land_listing(
            &mut loads,
            &mut children,
            &mut stale,
            &key,
            id,
            dir.clone(),
            vec![entry("src", true)],
        );

        stale.insert(key.clone());
        assert_eq!(
            children.get(id, &dir).map(Vec::len),
            Some(1),
            "the rows are still there to paint"
        );

        let current = children.get(id, &dir).is_some() && !stale.contains(&key);
        assert!(!current, "stale means re-ask");

        loads.begin(key.clone());
        land_listing(
            &mut loads,
            &mut children,
            &mut stale,
            &key,
            id,
            dir.clone(),
            vec![entry("src", true), entry("README", false)],
        );
        assert!(!stale.contains(&key), "the replacement clears the mark");
        assert_eq!(children.get(id, &dir).map(Vec::len), Some(2));
    }

    #[test]
    fn a_directorys_own_event_does_not_relist_it() {
        let batch: HashSet<PathBuf> = [
            PathBuf::from("/home/me/.claude.json"),
            PathBuf::from("/home/me"),
        ]
        .into_iter()
        .collect();

        let dirs = dirs_to_relist(&batch, false);
        assert!(
            !dirs.contains(Path::new("/home/me")),
            "the home listing is not re-fetched for a dot-file write"
        );
        assert!(dirs.contains(Path::new("/home")), "its parent is");

        let batch: HashSet<PathBuf> = [
            PathBuf::from("/home/me/notes.md"),
            PathBuf::from("/home/me"),
        ]
        .into_iter()
        .collect();
        assert!(dirs_to_relist(&batch, false).contains(Path::new("/home/me")));

        let batch: HashSet<PathBuf> = [PathBuf::from("/home/me/.claude.json")]
            .into_iter()
            .collect();
        assert!(dirs_to_relist(&batch, true).contains(Path::new("/home/me")));
        assert!(dirs_to_relist(&batch, false).is_empty());
    }

    #[test]
    fn an_unshown_dot_file_does_not_trigger_a_relist() {
        let hidden = Path::new("/home/me/.claude.json");
        let visible = Path::new("/home/me/src");
        assert!(!event_can_change_a_row(hidden, false));
        assert!(event_can_change_a_row(hidden, true));
        assert!(event_can_change_a_row(visible, false));
        assert!(!event_can_change_a_row(
            Path::new("/home/me/.config"),
            false
        ));
        assert!(event_can_change_a_row(Path::new("/home/me/.config"), true));
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

    fn tree_entry(path: &str, is_dir: bool, ignored: bool) -> TreeEntry {
        let path = PathBuf::from(path);
        TreeEntry {
            name: path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
            path,
            is_dir,
            ignored,
        }
    }

    fn one_repo(paths: &[(&str, DecoStatus)]) -> Decorations {
        let mut index = StatusIndex::default();
        for (path, status) in paths {
            index.insert(path, *status);
        }
        vec![(PathBuf::from("/repo"), Arc::new(index))]
    }

    #[test]
    fn a_row_is_keyed_by_where_it_sits_below_the_repository_root() {
        let root = Path::new("/repo");
        assert!(
            repo_relative(root, Path::new("/repo")).is_none(),
            "the root row has no key of its own"
        );
        assert_eq!(
            repo_relative(root, Path::new("/repo/README.md")).as_deref(),
            Some("README.md")
        );
        assert_eq!(
            repo_relative(root, Path::new("/repo/src/ui/file_tree.rs")).as_deref(),
            Some("src/ui/file_tree.rs")
        );
        assert!(
            repo_relative(root, Path::new("/elsewhere/a.rs")).is_none(),
            "a row outside the repository is not decorated"
        );
        assert!(
            repo_relative(root, Path::new("/repository/a.rs")).is_none(),
            "a shared text prefix is not a shared root"
        );
    }

    #[test]
    fn a_windows_path_is_keyed_with_forward_slashes() {
        assert_eq!(
            with_forward_slashes(r"src\ui\file_tree.rs", '\\'),
            "src/ui/file_tree.rs"
        );
        assert_eq!(with_forward_slashes("README.md", '\\'), "README.md");
        assert_eq!(
            with_forward_slashes("src/ui/file_tree.rs", '/'),
            "src/ui/file_tree.rs"
        );
        // The rows that exist in their thousands must not allocate a key.
        assert!(matches!(
            with_forward_slashes("src/ui/file_tree.rs", '/'),
            Cow::Borrowed(_)
        ));
        assert!(matches!(
            with_forward_slashes("README.md", '\\'),
            Cow::Borrowed(_)
        ));
    }

    #[test]
    fn every_status_gets_its_letter_and_its_own_name_shape() {
        // (status, letter, bold, struck through)
        let cases = [
            (DecoStatus::Conflict, "U", true, false),
            (DecoStatus::Deleted, "D", false, true),
            (DecoStatus::Added, "A", false, false),
            (DecoStatus::Untracked, "?", false, false),
            (DecoStatus::Modified, "M", false, false),
            (DecoStatus::Renamed, "R", false, false),
        ];
        for (status, letter, bold, strike) in cases {
            let deco = RowDeco::file(status);
            assert_eq!(deco.letter, letter, "{status:?}");
            assert_eq!(
                deco.tint,
                Some(status),
                "{status:?} colours the name through the shared table"
            );
            assert_eq!(deco.bold, bold, "{status:?}");
            assert_eq!(deco.strike, strike, "{status:?}");
            assert_eq!(deco.badge(), Some((letter, status)), "{status:?}");
        }

        let ignored = RowDeco::file(DecoStatus::Ignored);
        assert_eq!(ignored.letter, "");
        assert!(
            ignored.badge().is_none(),
            "a tree full of `!` is noise, not information"
        );
    }

    #[test]
    fn a_folder_is_only_ever_changed_or_conflicted_and_never_lettered() {
        assert_eq!(RowDeco::dir(DirRollup::default()), RowDeco::default());

        let changed = RowDeco::dir(DirRollup {
            changed: true,
            conflict: false,
        });
        assert_eq!(
            changed.tint,
            Some(DecoStatus::Modified),
            "the same warning a modified file wears"
        );
        assert_eq!(changed.letter, "");
        assert!(changed.badge().is_none(), "a folder is not `M`");

        let conflict = RowDeco::dir(DirRollup {
            changed: true,
            conflict: true,
        });
        assert_eq!(
            conflict.tint,
            Some(DecoStatus::Conflict),
            "a conflict below outranks a mere change below"
        );
        assert_eq!(conflict.letter, "");
        assert!(
            !conflict.bold,
            "the folder points; the file inside it shouts"
        );
    }

    #[test]
    fn dropping_the_file_map_leaves_the_folders_decorated() {
        let mut index = StatusIndex::default();
        index.insert("src/ui/a.rs", DecoStatus::Modified);
        index.drop_files();
        let decor: Decorations = vec![(PathBuf::from("/repo"), Arc::new(index))];

        assert_eq!(
            row_decoration(&decor, &tree_entry("/repo/src/ui/a.rs", false, false)),
            RowDeco::default(),
            "no letter survives the circuit breaker"
        );
        assert_eq!(
            row_decoration(&decor, &tree_entry("/repo/src", true, false)).tint,
            Some(DecoStatus::Modified),
            "but the folders still say where the work is"
        );
    }

    #[test]
    fn a_gitignored_row_is_left_with_the_styling_it_already_had() {
        let decor = one_repo(&[("target/debug/app", DecoStatus::Untracked)]);
        assert_eq!(
            row_decoration(&decor, &tree_entry("/repo/target/debug/app", false, false)).letter,
            "?",
            "the same row without the ignore flag is decorated"
        );
        assert_eq!(
            row_decoration(&decor, &tree_entry("/repo/target/debug/app", false, true)),
            RowDeco::default(),
            "italic and dim is the whole of what an ignored row says"
        );
        assert_eq!(
            row_decoration(&decor, &tree_entry("/repo/target", true, true)),
            RowDeco::default(),
            "and an ignored folder does not get a rollup colour either"
        );
    }

    #[test]
    fn the_innermost_repository_answers_for_its_own_rows() {
        let mut outer = StatusIndex::default();
        outer.insert("vendor/lib/a.rs", DecoStatus::Modified);
        let mut inner = StatusIndex::default();
        inner.insert("a.rs", DecoStatus::Conflict);
        let mut decor: Decorations = vec![
            (PathBuf::from("/repo"), Arc::new(outer)),
            (PathBuf::from("/repo/vendor/lib"), Arc::new(inner)),
        ];
        order_innermost_first(&mut decor);

        assert_eq!(
            row_decoration(&decor, &tree_entry("/repo/vendor/lib/a.rs", false, false)).letter,
            "U",
            "the submodule, not the repository holding it"
        );
        assert_eq!(
            row_decoration(&decor, &tree_entry("/repo/vendor", true, false)).tint,
            Some(DecoStatus::Modified),
            "the outer repository still rolls its own directories up"
        );
        assert_eq!(
            row_decoration(&decor, &tree_entry("/elsewhere/a.rs", false, false)),
            RowDeco::default()
        );
        assert_eq!(
            row_decoration(&decor, &tree_entry("/repo/README.md", false, false)),
            RowDeco::default(),
            "a clean tracked file is left alone"
        );
    }

    #[test]
    fn shell_quote_leaves_safe_paths_and_quotes_the_rest() {
        assert_eq!(shell_quote(Path::new("/a/b.txt")), "/a/b.txt");
        assert_eq!(shell_quote(Path::new("/a dir/f")), "'/a dir/f'");
        assert_eq!(shell_quote(Path::new("/a'b")), r"'/a'\''b'");
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
            !search.accept(first, true, vec![entry("stale.rs", false)]),
            "the overtaken walk's hits are dropped"
        );
        assert!(search.accept(second, true, vec![entry("foo.rs", false)]));
        assert_eq!(search.hits.len(), 1);

        let third = search
            .retarget("foo", true)
            .expect("showing dotfiles re-walks");
        assert_ne!(second, third);
        assert!(search.retarget("foo", true).is_none());

        assert!(search.retarget("", true).is_none());
        assert!(search.hits.is_empty());
        search.retarget("foo", true).expect("typing again walks");
        search.restart();
        assert!(search.retarget("foo", true).is_some(), "restart re-walks");
    }

    #[test]
    fn a_search_that_failed_is_not_a_search_with_no_hits() {
        let mut search = SearchState::default();
        let walk = search.retarget("foo", false).expect("a new query walks");
        assert!(search.accept(walk, false, Vec::new()));
        assert!(
            search.failed,
            "an empty list from a host that refused the walk is not an answer"
        );

        // And it is not carried past the query it belongs to.
        let next = search
            .retarget("food", false)
            .expect("a changed query walks");
        assert!(search.accept(next, true, vec![entry("food.rs", false)]));
        assert!(!search.failed);

        let last = search.retarget("foodie", false).expect("and again");
        assert!(search.accept(last, false, Vec::new()));
        assert!(search.failed);
        search.retarget("", false);
        assert!(!search.failed, "an emptied box has nothing to report");
    }

    #[test]
    fn the_tree_reads_the_same_listing_out_of_the_host() {
        let host = tty7_core::host::local::LocalHost::new();
        let tmp = std::env::temp_dir().join(format!("tty7-tree-host-{}", std::process::id()));
        let _ = host.remove(&tmp, true);
        host.create_dir(&tmp.join(".git"), true).unwrap();
        host.create_dir(&tmp.join("src"), true).unwrap();
        host.write_file(&tmp.join(".gitignore"), b"*.log\nbuild/\n")
            .unwrap();
        host.write_file(&tmp.join("src/.gitignore"), b"!keep.log\n")
            .unwrap();
        host.write_file(&tmp.join("drop.log"), b"").unwrap();
        host.write_file(&tmp.join("src/keep.log"), b"").unwrap();
        host.write_file(&tmp.join("src/main.rs"), b"").unwrap();

        let list = |dir: &Path| -> Vec<TreeEntry> {
            host.read_dir(dir, Some(&tmp))
                .unwrap()
                .into_iter()
                .map(|e| TreeEntry {
                    path: host.join(dir, &e.name),
                    name: e.name,
                    is_dir: e.is_dir,
                    ignored: e.ignored,
                })
                .collect()
        };
        let ignored = |entries: &[TreeEntry], name: &str| {
            entries
                .iter()
                .find(|e| e.name == name)
                .unwrap_or_else(|| panic!("{name} missing"))
                .ignored
        };
        let top = list(&tmp);
        assert!(ignored(&top, "drop.log"));
        assert!(ignored(&top, ".git"));
        assert!(!ignored(&top, "src"));
        assert_eq!(
            top.iter().find(|e| e.name == "src").unwrap().path,
            tmp.join("src"),
            "entries carry a full path, rebuilt with the host's separator"
        );
        let nested = list(&tmp.join("src"));
        assert!(!ignored(&nested, "keep.log"), "whitelist un-ignores");
        assert!(!ignored(&nested, "main.rs"));

        let hits = host
            .search(
                std::slice::from_ref(&tmp),
                "log",
                SEARCH_LIMIT,
                SEARCH_MAX_DIRS,
                false,
            )
            .unwrap();
        let names: Vec<&str> = hits.iter().map(|h| h.name.as_str()).collect();
        assert_eq!(names, vec!["keep.log"], "ignored hits stay out of search");

        let hidden = host
            .search(
                std::slice::from_ref(&tmp),
                "log",
                SEARCH_LIMIT,
                SEARCH_MAX_DIRS,
                true,
            )
            .unwrap();
        let mut names: Vec<&str> = hidden.iter().map(|h| h.name.as_str()).collect();
        names.sort_unstable();
        assert_eq!(names, vec!["drop.log", "keep.log"]);

        let _ = host.remove(&tmp, true);
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_cycle_cannot_make_the_search_walk_forever() {
        let host = tty7_core::host::local::LocalHost::new();
        let tmp = std::env::temp_dir().join(format!("tty7-tree-loop-{}", std::process::id()));
        let _ = host.remove(&tmp, true);
        host.create_dir(&tmp, true).unwrap();
        host.write_file(&tmp.join("needle.rs"), b"").unwrap();
        host.create_dir(&tmp.join("a"), true).unwrap();
        std::os::unix::fs::symlink(tmp.join("a"), tmp.join("a/loop")).unwrap();

        let hits = host
            .search(
                std::slice::from_ref(&tmp),
                "needle",
                SEARCH_LIMIT,
                SEARCH_MAX_DIRS,
                false,
            )
            .expect("the walk terminates rather than recursing forever");
        assert_eq!(
            hits.iter().map(|h| h.name.as_str()).collect::<Vec<_>>(),
            vec!["needle.rs"],
            "breadth-first order finds the shallow hit before the cycle deepens"
        );

        let listed = host.read_dir(&tmp.join("a"), Some(&tmp)).unwrap();
        let link = listed.iter().find(|e| e.name == "loop").expect("link");
        assert!(link.is_dir, "a link to a directory expands as one");
        assert!(link.is_symlink);

        let _ = host.remove(&tmp, true);
    }

    #[test]
    fn a_rejected_write_drops_the_row_it_guessed() {
        let host = HostId::LOCAL;
        let dir = PathBuf::from("/x");
        let mut children: ByHost<PathBuf, Vec<TreeEntry>> = ByHost::default();
        let names = |children: &ByHost<PathBuf, Vec<TreeEntry>>| -> Vec<String> {
            children
                .get(host, &dir)
                .map(|v| v.iter().map(|e| e.name.clone()).collect())
                .unwrap_or_default()
        };
        let seed = |children: &mut ByHost<PathBuf, Vec<TreeEntry>>| {
            children.insert(host, dir.clone(), vec![entry("b.rs", false)]);
        };

        seed(&mut children);
        let new = entry("a.rs", false);
        let before = optimistic_write(&mut children, host, &dir, &TreeWrite::NewFile, &new);
        assert!(before.is_some());
        assert_eq!(names(&children), vec!["a.rs", "b.rs"]);

        seed(&mut children);
        let renamed = TreeEntry {
            name: "z.rs".into(),
            path: PathBuf::from("/x/z.rs"),
            is_dir: false,
            ignored: false,
        };
        optimistic_write(
            &mut children,
            host,
            &dir,
            &TreeWrite::Rename {
                from: PathBuf::from("/x/b.rs"),
            },
            &renamed,
        );
        assert_eq!(names(&children), vec!["z.rs"]);

        seed(&mut children);
        let doomed = entry("b.rs", false);
        optimistic_write(&mut children, host, &dir, &TreeWrite::Delete, &doomed);
        assert!(names(&children).is_empty());

        seed(&mut children);
        let before = optimistic_write(&mut children, host, &dir, &TreeWrite::NewFile, &new);
        rollback_write(&mut children, host, &dir, before);
        assert!(
            children.get(host, &dir).is_none(),
            "a failed write leaves the directory to relist"
        );

        seed(&mut children);
        let before = optimistic_write(&mut children, host, &dir, &TreeWrite::NewFile, &new);
        children.insert(host, dir.clone(), vec![entry("fresh.rs", false)]);
        rollback_write(&mut children, host, &dir, before);
        assert!(
            children.get(host, &dir).is_none(),
            "the stale snapshot never overwrites a newer listing"
        );

        let other = PathBuf::from("/y");
        let before = optimistic_write(&mut children, host, &other, &TreeWrite::NewFile, &new);
        assert!(before.is_none());
        assert!(children.get(host, &other).is_none());
        rollback_write(&mut children, host, &other, before);
        assert!(children.get(host, &other).is_none());
    }
}

#[cfg(all(test, unix))]
mod render_idle_gpui_tests {
    use super::*;
    use crate::daemon::protocol::DaemonMsg;
    use crate::ui::app::{render_probe, test_window};
    use gpui::{Entity, TestAppContext, VisualTestContext};
    use tty7_core::core::config::RightPanelTab;

    const BUDGET: u64 = 200;

    pub(super) fn serial() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(super) fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("tty7-idle-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::canonicalize(&dir).unwrap()
    }

    pub(super) fn files_panel_on(
        cx: &mut TestAppContext,
        root: &Path,
    ) -> (
        Entity<Tty7App>,
        VisualTestContext,
        std::os::unix::net::UnixStream,
    ) {
        let (app, mut vcx, mut pane) = test_window::harness_with_pane(cx);
        DaemonMsg::Cwd(root.to_path_buf())
            .encode(&mut pane)
            .expect("the pane's socket takes the cwd");
        app.update_in(&mut vcx, |app, _, cx| {
            app.right_panel_visible = true;
            app.right_panel_tab = RightPanelTab::Files;
            cx.notify();
        });
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        loop {
            vcx.background_executor.run_until_parked();
            let rooted = app.update_in(&mut vcx, |app, window, cx| {
                app.file_tree_refresh_roots(window, cx);
                app.tab_code().map(|c| c.roots.clone()).unwrap_or_default()
                    == vec![root.to_path_buf()]
            });
            if rooted {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the pane never reported its cwd"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        loop {
            app.update_in(&mut vcx, |_, _, cx| cx.notify());
            vcx.background_executor.run_until_parked();
            let listed = app.update_in(&mut vcx, |app, _, _| {
                app.file_tree.children.get(HostId::LOCAL, root).is_some()
            });
            if listed {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the root was never listed"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        vcx.background_executor.run_until_parked();
        test_window::quiesce(&mut vcx, Some(root));
        (app, vcx, pane)
    }

    pub(super) fn rows(app: &Entity<Tty7App>, vcx: &mut VisualTestContext) -> usize {
        app.update_in(vcx, |app, _, _| {
            let code = app.tab_code().expect("panel state");
            app.file_tree
                .visible_rows(HostId::LOCAL, &code.roots, &code.expanded)
                .iter()
                .filter(|r| r.note.is_none())
                .count()
        })
    }

    fn fs_event(app: &Entity<Tty7App>, vcx: &mut VisualTestContext, path: &Path) {
        app.update_in(vcx, |app, _, cx| {
            app.file_tree_apply_fs_events(HostId::LOCAL, &HashSet::from([path.to_path_buf()]), cx);
        });
    }

    pub(super) fn settle(app: &Entity<Tty7App>, vcx: &mut VisualTestContext, root: &Path) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while std::time::Instant::now() < deadline {
            vcx.background_executor.run_until_parked();
            let quiet = app.update_in(vcx, |app, _, _| {
                app.file_tree.loads.is_empty()
                    && !app
                        .file_tree
                        .stale
                        .iter()
                        .any(|(_, dir)| dir.starts_with(root))
            });
            if quiet {
                vcx.background_executor.run_until_parked();
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("the tree never went quiet");
    }

    /// Runs git with the identity and signing pinned, so the test does not
    /// depend on whatever is in the developer's `~/.gitconfig`.
    fn git(root: &Path, args: &[&str]) -> bool {
        let mut full = vec![
            "-c",
            "user.name=tty7 test",
            "-c",
            "user.email=test@tty7.invalid",
            "-c",
            "commit.gpgsign=false",
        ];
        full.extend_from_slice(args);
        std::process::Command::new("git")
            .args(&full)
            .current_dir(root)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    /// The status probe only goes out from `render`, so this drives frames
    /// until the index it produces is on the global.
    fn scm_index(
        app: &Entity<Tty7App>,
        vcx: &mut VisualTestContext,
        root: &Path,
    ) -> Arc<StatusIndex> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            app.update_in(vcx, |_, _, cx| cx.notify());
            vcx.background_executor.run_until_parked();
            if let Some(index) = app.update_in(vcx, |_, _, cx| index_of(cx, HostId::LOCAL, root)) {
                return index;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the repository status never landed"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    fn draws_while_idle(vcx: &mut VisualTestContext) -> u64 {
        test_window::quiesce(vcx, None);
        render_probe::arm(BUDGET);
        vcx.background_executor.run_until_parked();
        vcx.executor()
            .advance_clock(std::time::Duration::from_secs(3));
        vcx.background_executor.run_until_parked();
        render_probe::arm(BUDGET);
        // No real-time exposure in the counted window, deliberately. The file
        // tree holds a real filesystem watch, so real time is a channel input
        // arrives on — and a test that spends it here is asking to be handed
        // some. What has to be waited out is waited out in `quiesce` above,
        // where a frame costs nothing.
        vcx.executor()
            .advance_clock(std::time::Duration::from_secs(9));
        vcx.background_executor.run_until_parked();
        render_probe::draws()
    }

    #[gpui::test]
    fn a_settled_files_panel_reaches_render_idle(cx: &mut TestAppContext) {
        let _serial = serial();
        let root = scratch("settled");
        std::fs::create_dir_all(root.join("src")).unwrap();
        for n in 0..12 {
            std::fs::write(root.join(format!("file{n:02}.rs")), "").unwrap();
        }
        let (app, mut vcx, _pane) = files_panel_on(cx, &root);
        assert!(rows(&app, &mut vcx) > 1, "the tree listed nothing");
        assert_eq!(draws_while_idle(&mut vcx), 0);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[gpui::test]
    fn a_decorated_tree_settles_and_then_reaches_render_idle(cx: &mut TestAppContext) {
        let _serial = serial();
        crate::core::config::pin_test_config_dir();
        let root = scratch("decorated");
        if !git(&root, &["init", "--quiet"]) {
            return; // no git on this machine
        }
        std::fs::write(root.join("tracked.rs"), "one\n").unwrap();
        assert!(git(&root, &["add", "-A"]));
        assert!(git(&root, &["commit", "--quiet", "-m", "base"]));
        std::fs::write(root.join("tracked.rs"), "one\ntwo\n").unwrap();
        std::fs::write(root.join("loose.rs"), "new\n").unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/deep.rs"), "new\n").unwrap();

        let (app, mut vcx, _pane) = files_panel_on(cx, &root);
        let decor: Decorations = vec![(root.clone(), scm_index(&app, &mut vcx, &root))];
        let deco = |name: &str, is_dir: bool| {
            row_decoration(
                &decor,
                &TreeEntry {
                    name: name.to_string(),
                    path: root.join(name),
                    is_dir,
                    ignored: false,
                },
            )
        };

        assert_eq!(deco("tracked.rs", false).letter, "M");
        assert_eq!(deco("loose.rs", false).letter, "?");
        assert_eq!(
            deco("src", true).tint,
            Some(DecoStatus::Modified),
            "the collapsed folder says there is work under it"
        );
        assert_eq!(
            deco("src", true).letter,
            "",
            "without pretending to be a file"
        );

        settle(&app, &mut vcx, &root);
        assert_eq!(draws_while_idle(&mut vcx), 0);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[gpui::test]
    fn a_settled_files_panel_on_an_empty_directory_reaches_render_idle(cx: &mut TestAppContext) {
        let _serial = serial();
        let root = scratch("empty");
        let (app, mut vcx, _pane) = files_panel_on(cx, &root);
        assert_eq!(rows(&app, &mut vcx), 1, "the root row and nothing else");
        assert_eq!(draws_while_idle(&mut vcx), 0);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[gpui::test]
    fn a_settled_files_panel_on_hidden_only_content_reaches_render_idle(cx: &mut TestAppContext) {
        let _serial = serial();
        let root = scratch("hidden");
        std::fs::write(root.join(".hidden"), "").unwrap();
        let (app, mut vcx, _pane) = files_panel_on(cx, &root);
        assert_eq!(rows(&app, &mut vcx), 1, "the dotfile is filtered out");
        assert_eq!(draws_while_idle(&mut vcx), 0);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[gpui::test]
    fn an_event_reaching_no_cached_listing_costs_no_frames(cx: &mut TestAppContext) {
        let _serial = serial();
        let root = scratch("unlisted");
        for n in 0..12 {
            std::fs::write(root.join(format!("file{n:02}.rs")), "").unwrap();
        }
        std::fs::create_dir_all(root.join("target/debug")).unwrap();
        let (app, mut vcx, _pane) = files_panel_on(cx, &root);
        let before = rows(&app, &mut vcx);

        render_probe::arm(BUDGET);
        for n in 0..5 {
            let path = root.join(format!("target/debug/artifact{n}.o"));
            std::fs::write(&path, "").unwrap();
            fs_event(&app, &mut vcx, &path);
            settle(&app, &mut vcx, &root);
        }
        assert_eq!(render_probe::draws(), 0, "nothing on screen changed");
        assert_eq!(rows(&app, &mut vcx), before);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[gpui::test]
    fn rewriting_a_file_in_a_displayed_directory_costs_no_frames(cx: &mut TestAppContext) {
        let _serial = serial();
        let root = scratch("rewrite");
        for n in 0..12 {
            std::fs::write(root.join(format!("file{n:02}.rs")), "").unwrap();
        }
        let (app, mut vcx, _pane) = files_panel_on(cx, &root);
        let before = rows(&app, &mut vcx);

        render_probe::arm(BUDGET);
        for n in 0..5 {
            let path = root.join("file00.rs");
            std::fs::write(&path, format!("line {n}")).unwrap();
            fs_event(&app, &mut vcx, &path);
            settle(&app, &mut vcx, &root);
        }
        assert_eq!(render_probe::draws(), 0, "the listing came back identical");
        assert_eq!(rows(&app, &mut vcx), before);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[gpui::test]
    fn a_real_change_still_reaches_the_panel(cx: &mut TestAppContext) {
        let _serial = serial();
        let root = scratch("realchange");
        for n in 0..12 {
            std::fs::write(root.join(format!("file{n:02}.rs")), "").unwrap();
        }
        let (app, mut vcx, _pane) = files_panel_on(cx, &root);
        let before = rows(&app, &mut vcx);

        let added = root.join("new.rs");
        std::fs::write(&added, "").unwrap();
        fs_event(&app, &mut vcx, &added);
        assert_eq!(
            rows(&app, &mut vcx),
            before,
            "the rows survive the refresh they triggered"
        );
        settle(&app, &mut vcx, &root);
        assert_eq!(rows(&app, &mut vcx), before + 1, "the new file shows up");

        std::fs::remove_file(&added).unwrap();
        fs_event(&app, &mut vcx, &added);
        settle(&app, &mut vcx, &root);
        assert_eq!(rows(&app, &mut vcx), before);
        assert_eq!(draws_while_idle(&mut vcx), 0);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[gpui::test]
    fn a_gitignore_that_governs_nothing_cached_costs_no_frames(cx: &mut TestAppContext) {
        let _serial = serial();
        let root = scratch("gitignore-unlisted");
        std::fs::write(root.join("main.rs"), "").unwrap();
        std::fs::create_dir_all(root.join("node_modules/pkg")).unwrap();
        let (app, mut vcx, _pane) = files_panel_on(cx, &root);

        render_probe::arm(BUDGET);
        for n in 0..5 {
            let path = root.join(format!("node_modules/pkg{n}/.gitignore"));
            fs_event(&app, &mut vcx, &path);
            settle(&app, &mut vcx, &root);
        }
        assert_eq!(render_probe::draws(), 0, "it cannot reach a cached listing");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[gpui::test]
    fn a_gitignore_in_the_displayed_tree_still_refreshes(cx: &mut TestAppContext) {
        let _serial = serial();
        let root = scratch("gitignore-displayed");
        for n in 0..12 {
            std::fs::write(root.join(format!("file{n:02}.rs")), "").unwrap();
        }
        std::fs::write(root.join(".gitignore"), "").unwrap();
        let (app, mut vcx, _pane) = files_panel_on(cx, &root);
        let before = rows(&app, &mut vcx);

        let ignore = root.join(".gitignore");
        std::fs::write(&ignore, "file00.rs\n").unwrap();
        fs_event(&app, &mut vcx, &ignore);
        let marked = app.update_in(&mut vcx, |app, _, _| {
            app.file_tree
                .stale
                .iter()
                .filter(|(_, dir)| dir.starts_with(&root))
                .count()
        });
        assert!(marked > 0, "the batch reached the tree");
        assert_eq!(rows(&app, &mut vcx), before, "rows stay while it re-reads");

        settle(&app, &mut vcx, &root);
        assert_eq!(rows(&app, &mut vcx), before);
        let left = app.update_in(&mut vcx, |app, _, _| {
            app.file_tree
                .stale
                .iter()
                .filter(|(_, dir)| dir.starts_with(&root))
                .count()
        });
        assert_eq!(left, 0, "every marked listing under the root was re-read");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[gpui::test]
    fn untracked_paths_leave_no_bookkeeping_behind(cx: &mut TestAppContext) {
        let _serial = serial();
        let root = scratch("bookkeeping");
        std::fs::write(root.join("main.rs"), "").unwrap();
        std::fs::create_dir_all(root.join("target/debug")).unwrap();
        let (app, mut vcx, _pane) = files_panel_on(cx, &root);

        for n in 0..50 {
            let path = root.join(format!("target/debug/obj{n}.o"));
            fs_event(&app, &mut vcx, &path);
        }
        settle(&app, &mut vcx, &root);
        let marks = app.update_in(&mut vcx, |app, _, _| {
            app.file_tree
                .stale
                .iter()
                .filter(|(_, dir)| dir.starts_with(&root))
                .count()
        });
        assert_eq!(marks, 0, "nothing the tree holds was reached");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[gpui::test]
    fn a_moved_repository_root_still_gets_its_frame(cx: &mut TestAppContext) {
        let _serial = serial();
        let root = scratch("repo-root");
        std::fs::write(root.join("main.rs"), "").unwrap();
        let (app, mut vcx, _pane) = files_panel_on(cx, &root);
        assert!(
            app.update_in(&mut vcx, |app, _, _| !app.file_tree.repo_roots.is_empty()),
            "the panel resolved its pane's root, so there is a cache to clear"
        );

        vcx.executor()
            .advance_clock(std::time::Duration::from_secs(3));
        vcx.background_executor.run_until_parked();
        render_probe::arm(BUDGET);
        vcx.executor()
            .advance_clock(std::time::Duration::from_secs(3));
        vcx.background_executor.run_until_parked();
        assert_eq!(render_probe::draws(), 0, "the window is at rest");

        fs_event(&app, &mut vcx, &root.join(".git"));
        vcx.background_executor.run_until_parked();
        assert!(
            render_probe::draws() > 0,
            "clearing the root cache asked for the paint that re-resolves it"
        );

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            vcx.background_executor.run_until_parked();
            if app.update_in(&mut vcx, |app, _, _| !app.file_tree.repo_roots.is_empty()) {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the cleared root cache was never re-resolved"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        settle(&app, &mut vcx, &root);
        assert_eq!(
            app.update_in(&mut vcx, |app, _, _| app
                .tab_code()
                .map(|c| c.roots.clone())
                .unwrap_or_default()),
            vec![root.clone()],
            "the tree is still rooted where it belongs"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[gpui::test]
    fn a_closed_panel_does_no_filesystem_work(cx: &mut TestAppContext) {
        let _serial = serial();
        let root = scratch("closed-panel");
        for n in 0..12 {
            std::fs::write(root.join(format!("file{n:02}.rs")), "").unwrap();
        }
        let (app, mut vcx, _pane) = files_panel_on(cx, &root);

        app.update_in(&mut vcx, |app, _, cx| {
            app.right_panel_visible = false;
            cx.notify();
        });
        vcx.background_executor.run_until_parked();

        let path = root.join("file00.rs");
        std::fs::write(&path, "changed").unwrap();
        fs_event(&app, &mut vcx, &path);
        let until = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while std::time::Instant::now() < until {
            vcx.background_executor.run_until_parked();
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let (in_flight, marked) = app.update_in(&mut vcx, |app, _, _| {
            (
                app.file_tree.loads.len(),
                app.file_tree
                    .stale
                    .iter()
                    .filter(|(_, dir)| dir.starts_with(&root))
                    .count(),
            )
        });
        assert_eq!(in_flight, 0, "nothing was asked of the host");
        assert!(marked > 0, "but the change was recorded");

        app.update_in(&mut vcx, |app, _, cx| {
            app.right_panel_visible = true;
            cx.notify();
        });
        settle(&app, &mut vcx, &root);
        let left = app.update_in(&mut vcx, |app, _, _| {
            app.file_tree
                .stale
                .iter()
                .filter(|(_, dir)| dir.starts_with(&root))
                .count()
        });
        assert_eq!(left, 0, "the marked listing was re-read on reopening");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[gpui::test]
    fn a_searching_tree_does_no_filesystem_work(cx: &mut TestAppContext) {
        let _serial = serial();
        let root = scratch("searching-tree");
        for n in 0..12 {
            std::fs::write(root.join(format!("file{n:02}.rs")), "").unwrap();
        }
        let (app, mut vcx, _pane) = files_panel_on(cx, &root);

        app.update_in(&mut vcx, |app, window, cx| {
            app.file_search
                .update(cx, |st, cx| st.set_value("file0", window, cx));
            cx.notify();
        });
        vcx.background_executor.run_until_parked();
        assert!(
            app.update_in(&mut vcx, |app, _, cx| app.file_tree_searching(cx)
                && !app.file_tree_listings_on_screen(cx)),
            "the column is drawn, and what it is drawing is not the listings"
        );

        let path = root.join("file00.rs");
        std::fs::write(&path, "changed").unwrap();
        fs_event(&app, &mut vcx, &path);
        let until = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while std::time::Instant::now() < until {
            vcx.background_executor.run_until_parked();
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let (in_flight, marked) = app.update_in(&mut vcx, |app, _, _| {
            (
                app.file_tree.loads.len(),
                app.file_tree
                    .stale
                    .iter()
                    .filter(|(_, dir)| dir.starts_with(&root))
                    .count(),
            )
        });
        assert_eq!(in_flight, 0, "nothing was asked of the host");
        assert!(marked > 0, "but the change was recorded");

        app.update_in(&mut vcx, |app, window, cx| {
            app.file_search
                .update(cx, |st, cx| st.set_value("", window, cx));
            cx.notify();
        });
        settle(&app, &mut vcx, &root);
        let left = app.update_in(&mut vcx, |app, _, _| {
            app.file_tree
                .stale
                .iter()
                .filter(|(_, dir)| dir.starts_with(&root))
                .count()
        });
        assert_eq!(left, 0, "the marked listing was re-read on clearing");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[gpui::test]
    fn the_sftp_browser_holding_the_column_counts_as_not_drawn(cx: &mut TestAppContext) {
        let _serial = serial();
        let root = scratch("sftp-column");
        for n in 0..12 {
            std::fs::write(root.join(format!("file{n:02}.rs")), "").unwrap();
        }
        let (app, mut vcx, _pane) = files_panel_on(cx, &root);
        let path = root.join("file00.rs");
        std::fs::write(&path, "changed").unwrap();

        let (on_screen, in_flight, marked) = app.update_in(&mut vcx, |app, _, cx| {
            app.sftp_panel.open_pane_id = Some(7);
            let on_screen = app.file_tree_on_screen(cx);
            app.file_tree_apply_fs_events(HostId::LOCAL, &HashSet::from([path.clone()]), cx);
            (
                on_screen,
                app.file_tree.loads.len(),
                app.file_tree
                    .stale
                    .iter()
                    .filter(|(_, dir)| dir.starts_with(&root))
                    .count(),
            )
        });
        assert!(!on_screen, "the SFTP browser has the column, not the tree");
        assert_eq!(in_flight, 0, "so nothing was asked of the host");
        assert!(marked > 0, "but the change was recorded");

        app.update_in(&mut vcx, |app, _, cx| {
            app.sftp_panel.open_pane_id = None;
            cx.notify();
        });
        settle(&app, &mut vcx, &root);
        let left = app.update_in(&mut vcx, |app, _, _| {
            app.file_tree
                .stale
                .iter()
                .filter(|(_, dir)| dir.starts_with(&root))
                .count()
        });
        assert_eq!(left, 0, "the marked listing was re-read once it came back");
        let _ = std::fs::remove_dir_all(&root);
    }
}

/// The drop end of the panel, driven through the real app: the copy runs on a
/// `HostOps` worker and the tree has to catch up with what it wrote.
///
/// What these cannot reach is the hit test — whether the row under the cursor
/// is the one that gets the drop is decided by gpui's hitbox stack, and there
/// is no headless way to put a cursor over a row.
#[cfg(all(test, unix))]
mod drop_gpui_tests {
    use super::render_idle_gpui_tests::{files_panel_on, rows, scratch, serial, settle};
    use super::*;
    use gpui::{TestAppContext, VisualTestContext};

    /// The copy runs on a `HostOps` worker — a real OS thread the test
    /// executor does not own, so parking it proves nothing about whether the
    /// worker is done. This waits for the result instead of assuming it.
    fn wait_until(
        vcx: &mut VisualTestContext,
        what: &str,
        mut done: impl FnMut(&mut VisualTestContext) -> bool,
    ) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        loop {
            vcx.background_executor.run_until_parked();
            if done(vcx) {
                return;
            }
            assert!(std::time::Instant::now() < deadline, "{what}");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    #[gpui::test]
    fn a_dropped_file_is_copied_in_and_shows_up_in_the_tree(cx: &mut TestAppContext) {
        let _serial = serial();
        let root = scratch("drop-lands");
        let from = scratch("drop-source");
        std::fs::write(from.join("note.txt"), "hello").unwrap();
        let (app, mut vcx, _pane) = files_panel_on(cx, &root);
        let before = rows(&app, &mut vcx);

        app.update_in(&mut vcx, |app, window, cx| {
            app.file_tree_drop_paths(vec![from.join("note.txt")], root.clone(), window, cx);
        });
        wait_until(&mut vcx, "the copy never landed", |_| {
            root.join("note.txt").exists()
        });
        settle(&app, &mut vcx, &root);

        assert_eq!(
            std::fs::read_to_string(root.join("note.txt")).unwrap(),
            "hello"
        );
        assert_eq!(rows(&app, &mut vcx), before + 1, "the tree never caught up");
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&from);
    }

    #[gpui::test]
    fn a_drop_over_a_name_that_is_taken_asks_before_it_writes(cx: &mut TestAppContext) {
        let _serial = serial();
        let root = scratch("drop-conflict-no");
        std::fs::write(root.join("note.txt"), "old").unwrap();
        let from = scratch("drop-conflict-no-source");
        std::fs::write(from.join("note.txt"), "new").unwrap();
        let (app, mut vcx, _pane) = files_panel_on(cx, &root);

        app.update_in(&mut vcx, |app, window, cx| {
            app.file_tree_drop_paths(vec![from.join("note.txt")], root.clone(), window, cx);
        });
        wait_until(&mut vcx, "it overwrote without asking", |vcx| {
            vcx.has_pending_prompt()
        });
        vcx.simulate_prompt_answer(t(L10nKey::Cancel));
        settle(&app, &mut vcx, &root);

        assert_eq!(
            std::fs::read_to_string(root.join("note.txt")).unwrap(),
            "old",
            "answering no still replaced the file"
        );
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&from);
    }

    #[gpui::test]
    fn answering_yes_replaces_what_was_there(cx: &mut TestAppContext) {
        let _serial = serial();
        let root = scratch("drop-conflict-yes");
        std::fs::write(root.join("note.txt"), "old").unwrap();
        let from = scratch("drop-conflict-yes-source");
        std::fs::write(from.join("note.txt"), "new").unwrap();
        let (app, mut vcx, _pane) = files_panel_on(cx, &root);

        app.update_in(&mut vcx, |app, window, cx| {
            app.file_tree_drop_paths(vec![from.join("note.txt")], root.clone(), window, cx);
        });
        wait_until(&mut vcx, "it never asked", |vcx| vcx.has_pending_prompt());
        vcx.simulate_prompt_answer(t(L10nKey::FileDropReplace));
        wait_until(&mut vcx, "the replacement never landed", |_| {
            std::fs::read_to_string(root.join("note.txt")).is_ok_and(|s| s == "new")
        });
        settle(&app, &mut vcx, &root);

        assert_eq!(
            std::fs::read_to_string(root.join("note.txt")).unwrap(),
            "new"
        );
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&from);
    }
}
