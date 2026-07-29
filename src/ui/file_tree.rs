//! Local project file tree (the right panel's Files tab — see
//! `right_panel::render_panel_files` for the column that hosts it).
//!
//! Modelled on Warp's Project Explorer: lazily-loaded directories, gitignore
//! awareness (ignored entries render dimmed, not hidden), a filesystem watcher
//! that keeps listings fresh, keyboard navigation, inline new-file / rename
//! editing, and a per-row context menu (open / cd / reveal / copy path /
//! delete / attach to a coding agent). Roots come from the active tab's panes:
//! each pane's cwd resolves to its repository root (walk up to `.git`), so a
//! tab whose panes sit in two repos shows both as top-level roots.
//!
//! The panel is a lazy tree over a [`Host`] — the abstraction that makes the
//! same tree work against this machine or a remote one. Listings are cached per
//! `(host, directory)` and invalidated by watcher events, so a huge repo only
//! ever pays for the directories actually expanded.
//!
//! The watch is scoped the same way: **non-recursive**, over the roots plus the
//! expanded directories and nothing else — see
//! [`FileTreeState::sync_watch`]. So `target/`, `node_modules` and the inside of
//! `.git` produce no events at all unless the user has expanded them, and what
//! does arrive names a directory the tree is displaying. Anything reasoning
//! about "what the watcher reports" starts there, not from a recursive walk of
//! the root.
//!
//! **Nothing here touches the filesystem directly.** Every read, every write and
//! every `git` invocation goes through [`HostOps`], which runs the blocking
//! `Host` call on the background executor and lands the answer on the UI thread.
//! Render only ever reads the cache, and a miss turns into a queued load whose
//! answer arrives with a `cx.notify()` a frame or more later. A directory the
//! user just expanded is therefore empty for one paint, which is what the cache
//! miss costs and what every other editor does — far better than stalling the
//! frame on a cold `.gitignore` chain, on a 2000-directory walk, or (remotely)
//! on a network round trip.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::core::config::RightPanelTab;
use crate::ui::app::Tty7App;
use crate::ui::host_ops::{ByHost, HostId, HostOps, InFlight, SharedHost, WatchSub};
use crate::ui::host_registry::HostRegistry;
use gpui::prelude::*;
use gpui::{
    AnyElement, App, Context, Entity, ExternalPaths, FocusHandle, KeyDownEvent, MouseButton,
    PromptLevel, SharedString, Subscription, Window, div, px,
};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::menu::{ContextMenuExt as _, PopupMenu, PopupMenuItem};
use gpui_component::{ActiveTheme as _, Icon, IconName, Sizable as _, h_flex, v_flex};

/// Per-level indent (px) for nested rows.
const INDENT: f32 = 14.0;

/// Debounce for watcher-driven refreshes (same rationale as the config
/// hot-reload: coalesce a save burst into one reload).
const REFRESH_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(200);

/// Debounce for the Files-tab search box. Each query walks up to
/// [`SEARCH_MAX_DIRS`] directories, so only the pause after the last keystroke
/// should pay for a walk — typing "src" otherwise buys three of them.
const SEARCH_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(200);

/// Hits a search stops at, so a query like "e" can't walk a whole monorepo.
const SEARCH_LIMIT: usize = 200;

/// Directories a search visits even if nothing matches, so a typo can't turn
/// into a full-disk crawl. Also what bounds a walk through a symlink cycle:
/// `Host::read_dir` follows links, so `a/link -> a` yields an unbounded chain of
/// distinct paths and this budget — not cycle detection — is what ends it.
const SEARCH_MAX_DIRS: usize = 2000;

/// One directory entry in a cached listing.
///
/// `PartialEq` so a landed relist can tell "this directory changed" from "a file
/// in it was rewritten": the watcher reports both, and only the first is worth a
/// repaint (issue #243).
#[derive(Clone, PartialEq)]
pub(crate) struct TreeEntry {
    pub name: String,
    pub path: PathBuf,
    pub is_dir: bool,
    /// Matched by the gitignore chain (or is `.git` itself): rendered dimmed.
    pub ignored: bool,
}

/// What a landed listing means for the caller: whether to go round again, and
/// whether it is worth a repaint.
///
/// Two separate questions. A listing superseded in flight has to be re-read
/// whatever it contained, and one that came back identical to what is on screen
/// is not worth a frame however current it is.
struct Landed {
    /// The listing was superseded while it flew, so ask again.
    superseded: bool,
    /// It differs from what the tree was already showing.
    changed: bool,
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

/// A directory key: which machine, and which path on it.
///
/// The pairing is the point. `/home/me/proj` exists on the laptop *and* on the
/// remote box, and a cache keyed by path alone would happily serve one
/// machine's listing for the other's directory.
type DirKey = (HostId, PathBuf);

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
    /// only thing render reads — a miss is a queued load, never a host call.
    children: ByHost<PathBuf, Vec<TreeEntry>>,
    loads: InFlight<DirKey>,
    /// Cached listings a watcher event has outdated: still on screen, queued to
    /// be relisted, replaced only when the new listing lands.
    ///
    /// Dropping them at invalidation time instead is what a filesystem change
    /// used to do, and it is invisible locally — the relist is microseconds. On
    /// a remote host it is a round trip during which the directory has no
    /// listing at all, so every row under it leaves the screen and comes back:
    /// one file rewritten a few times a second makes the whole tree strobe.
    stale: HashSet<DirKey>,
    /// Each pane cwd resolved to its repository root, so deriving the root set
    /// is a cache read rather than a walk up the tree per frame.
    repo_roots: ByHost<PathBuf, PathBuf>,
    repo_root_loads: InFlight<DirKey>,
    search: SearchState,
    pub(crate) show_hidden: bool,
    pub(crate) editing: Option<TreeEdit>,
    editing_subs: Vec<Subscription>,
    /// The live watch over the union of every tab's roots and expanded
    /// directories.
    ///
    /// One long-lived subscription whose *set* moves, rather than a watcher
    /// rebuilt per change: on a remote host a rebuild is a round trip plus a
    /// server-side watcher torn down and recreated for every disclosure
    /// triangle. `Arc` because `set_dirs` is itself a host call and has to be
    /// handed to the background executor.
    watch: Option<Arc<WatchSub>>,
    /// The host `watch` was opened against, kept so a subscription is never
    /// reused across a different one.
    ///
    /// A `HostId` is not enough to tell them apart: reconnecting removes the
    /// dead `RemoteHost` and inserts a fresh one under the *same* id, so the id
    /// matches while the `ControlClient` behind the old subscription is gone.
    /// Compared by pointer, which distinguishes both that and an outright
    /// switch to another machine.
    watch_host: Option<SharedHost>,
    /// A subscription is being opened. Without this, render would ask for one
    /// per frame until the first answer lands.
    watch_opening: bool,
    /// A `set_dirs` is in flight, and whether the set moved again while it was.
    ///
    /// `set_dirs` replaces the watched set wholesale, so two in flight resolve
    /// by arrival order rather than issue order — and the loser strands the
    /// watch on a stale set *permanently*, since the caller only re-issues when
    /// the desired set changes. Expanding two directories a frame apart is
    /// enough to hit it locally; over an RPC, out-of-order is the normal case.
    watch_busy: bool,
    watch_dirty: bool,
    /// What the watch currently spans. The union can move without the active
    /// tab's roots moving — closing a tab drops roots from it — so the sync
    /// check compares this rather than trusting the per-tab comparison.
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
                // Only coalesce batches from the same host: a path means nothing
                // without the machine it is on.
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

    /// Point the watch at `dirs` — roots plus every expanded directory, since
    /// the subscription is non-recursive and only the directories on screen can
    /// produce a visible change.
    ///
    /// Opens the subscription on first use and moves its set thereafter. Both
    /// are blocking host calls, so both go through [`HostOps`].
    fn sync_watch(&mut self, host: SharedHost, dirs: HashSet<PathBuf>, cx: &mut Context<Tty7App>) {
        self.watched = dirs;
        let want: Vec<PathBuf> = self.watched.iter().cloned().collect();
        // A subscription belongs to the host that opened it. Reconnecting drops
        // the dead `RemoteHost` and inserts a fresh one under the same
        // `HostId`, and adopting another workspace can change the host outright
        // — in both cases the subscription here is over a `ControlClient` that
        // is gone. `set_dirs` on it then fails with `ConnectionReset`, which is
        // warned and dropped, and nothing ever opens a new one: after the first
        // reconnect of a remote workspace the tree stops seeing changes made on
        // the far side for the rest of the window's life.
        if !self
            .watch_host
            .as_ref()
            .is_some_and(|opened_with| Arc::ptr_eq(opened_with, &host))
        {
            // Dropping the subscription is what unsubscribes, on both sides.
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
            // The landing re-reads `watched`, so a set that moves while the
            // subscription is being opened is applied when it arrives.
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
                // The receiver is cloned out so the pump owns a handle
                // independent of the subscription the state holds.
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
                // The set may have moved while the subscription was opening.
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

    /// Ask for a listing of every root and expanded directory that isn't cached
    /// yet. Called from render — so it must stay map lookups — and from the
    /// watcher callback, which re-reads what it just marked instead of asking
    /// for a paint to do it. Either way the `read_dir` runs on the background
    /// executor and the answer arrives with a `cx.notify()`, which is why a
    /// just-expanded directory fills in on the next frame rather than this one.
    ///
    /// Both callers first check that the listings are actually being drawn —
    /// see [`file_tree_listings_on_screen`](Tty7App::file_tree_listings_on_screen).
    fn request_loads(
        &mut self,
        host: &SharedHost,
        roots: &[PathBuf],
        expanded: &HashSet<PathBuf>,
        cx: &mut Context<Tty7App>,
    ) {
        // Roots always list; expanded dirs list on demand.
        for root in roots {
            self.request_load(host, root.clone(), root.clone(), cx);
            for dir in expanded {
                if dir.starts_with(root) {
                    self.request_load(host, dir.clone(), root.clone(), cx);
                }
            }
        }
    }

    /// Spawn one directory listing, unless it's already cached (and current) or
    /// already out.
    ///
    /// A cached-but-stale directory does re-ask: its rows stay on screen from
    /// the old listing while the new one flies, which is the whole point of
    /// keeping it.
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

    /// The listing itself, with the "should we?" already decided — so the
    /// landing can go round again without re-testing a cache it just filled.
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
                    // An unreadable directory lists as empty, exactly as it
                    // always has: the row stays, shows nothing under it, and
                    // does not re-ask every frame.
                    let entries = h.read_dir(&dir, Some(&root)).unwrap_or_default();
                    entries
                        .into_iter()
                        .map(|e| TreeEntry {
                            // `Host::join`, not `PathBuf::join`: a Windows
                            // client joining a remote POSIX path would
                            // otherwise produce `/home/me\src`.
                            path: h.join(&dir, &e.name),
                            name: e.name,
                            is_dir: e.is_dir,
                            ignored: e.ignored,
                        })
                        .collect::<Vec<_>>()
                }
            },
            move |app, entries, cx| {
                let landed = app.file_tree.land_load(&key, id, dir.clone(), entries);
                // Only a listing that came back *different* is worth a frame.
                if landed.changed {
                    cx.notify();
                }
                if !landed.superseded {
                    return;
                }
                // Superseded: the snapshot stays on screen, and we go round
                // again so it converges. Only while the tree is still pointed
                // at the same machine — a workspace that moved on has no use
                // for this directory.
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

    /// Retire a listing and put it in the cache.
    ///
    /// The listing lands **either way**. One that was superseded is still a
    /// real snapshot of that directory, and one change out of date beats
    /// nothing at all.
    ///
    /// Throwing it away instead starves any directory that changes faster than
    /// the round trip: every answer arrives already stale, the cache never
    /// fills, and the rows blink out on every paint. Locally that window is
    /// microseconds and the case never arises. Over an SSH link it is the whole
    /// round trip, so a single file rewritten a few times a second is enough —
    /// a coding agent's `~/.claude.json` is exactly that, and it made a remote
    /// tree rooted at `$HOME` flicker at the link's round-trip rate.
    fn land_load(
        &mut self,
        key: &DirKey,
        id: HostId,
        dir: PathBuf,
        entries: Vec<TreeEntry>,
    ) -> Landed {
        // Asked before the insert, because the insert is what destroys the
        // answer. A relist that read back exactly what is already on screen has
        // nothing to show, and the watcher reports a file's *contents* changing
        // as readily as an entry appearing — so a build writing into a
        // directory the tree is displaying would otherwise repaint the window
        // once per `REFRESH_DEBOUNCE` to draw the same rows again (#243).
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

    /// Point the search at `query` (empty = not searching), starting a
    /// debounced background walk when it isn't the one already in flight.
    /// Called from render, so the steady state is one string comparison.
    fn sync_search(&mut self, query: &str, roots: &[PathBuf], cx: &mut Context<Tty7App>) {
        let Some(generation) = self.search.retarget(query, self.show_hidden) else {
            return;
        };
        let show_hidden = self.show_hidden;
        let (query, roots) = (query.to_string(), roots.to_vec());
        cx.spawn(async move |app, cx| {
            cx.background_executor().timer(SEARCH_DEBOUNCE).await;
            // Another keystroke during the wait retargeted the search: bow out
            // before touching the host at all, which is the point of the wait.
            let _ = app.update(cx, |app, cx| {
                if app.file_tree.search.generation != generation {
                    return;
                }
                // The whole walk runs host-side in one call. Listing per
                // directory from here would be up to `SEARCH_MAX_DIRS` round
                // trips, which across an ocean is several minutes.
                let Some(host) = app.active_host(cx) else {
                    return;
                };
                HostOps::run(
                    host,
                    cx,
                    move |h| {
                        h.search(&roots, &query, SEARCH_LIMIT, SEARCH_MAX_DIRS, show_hidden)
                            .unwrap_or_default()
                            .into_iter()
                            .map(|hit| TreeEntry {
                                name: hit.name,
                                path: hit.path,
                                is_dir: hit.is_dir,
                                ignored: hit.ignored,
                            })
                            .collect::<Vec<_>>()
                    },
                    move |app, hits, cx| {
                        if app.file_tree.search.accept(generation, hits) {
                            cx.notify();
                        }
                    },
                );
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
                self.flatten_dir(host, &e.path, depth + 1, expanded, out);
            }
        }
    }

    /// Mark the cached listing for `dir` for a refresh after a change under it.
    /// Returns whether there was anything to mark.
    ///
    /// The listing stays until its replacement lands — see [`FileTreeState::stale`].
    ///
    /// The return value is what keeps a batch that reached nothing from
    /// repainting (issue #243). The subscription is non-recursive over the
    /// roots plus the expanded directories (see
    /// [`sync_watch`](FileTreeState::sync_watch)), so nearly everything that
    /// arrives *does* name a directory the tree holds — but not all of it: a
    /// watched directory whose listing never landed (a read that failed, a root
    /// removed underneath) is neither cached nor pending, and relisting for it
    /// buys a round trip and a frame for rows that are not on screen.
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

    /// Whether a `.gitignore` in this batch can reach anything the tree holds.
    ///
    /// A `.gitignore` at `D/.gitignore` governs `D` and everything below it and
    /// nowhere else, so it matters only when some directory the tree caches or
    /// is loading sits under `D`. A correctness guard on the branch, not a
    /// throughput one: [`invalidate_all`](FileTreeState::invalidate_all) marks
    /// *every* cached listing and restarts the search, and taking that for a
    /// file that cannot govern anything the tree holds is work with no possible
    /// visible result. A non-recursive watch makes such a batch rare — the
    /// `.gitignore` has to be a direct child of a watched directory to arrive at
    /// all — but "rare" is not "never": a watched directory whose own listing
    /// failed holds nothing, and neither does one still in flight when the
    /// deeper cache is empty.
    ///
    /// The test is complete as well as safe: any directory whose matchers could
    /// be cached is an ancestor of a directory that was successfully listed, and
    /// that one is in `children`.
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

    /// Mark every listing for a refresh — for the changes no smaller invalidation covers: a
    /// `.gitignore` edit (its patterns reach any depth below it) or a new root
    /// set.
    ///
    /// The compiled matchers themselves are no longer ours to clear: they live
    /// in the host, which drops them from inside its own watcher when a
    /// `.gitignore` moves. That is what gives a remote client the same
    /// invalidation for free — the server's host is the one watching.
    /// Deliberately leaves the resolved repository roots alone. Nothing this
    /// covers can change where a repository starts, and clearing them here
    /// would have the root derivation re-resolve every cwd immediately after
    /// installing the roots that triggered it.
    fn invalidate_all(&mut self) {
        self.stale
            .extend(self.children.keys().map(|(host, dir)| (host, dir.clone())));
        self.loads.invalidate_all();
        self.search.restart();
    }

    /// Forget which repository each cwd belongs to — for a `.git` appearing or
    /// disappearing, the only thing that can move a repository root.
    ///
    /// Reaches less far than it sounds: the roots a tab is *showing* live in
    /// `TabCode::roots` and are re-derived only when they are empty, on a tab
    /// switch, or on a panel toggle. So this makes the next such derivation
    /// correct rather than relocating a root under a tab that is already open —
    /// which is also what the synchronous version it replaced did.
    ///
    /// Returns whether anything was actually forgotten. Nothing here re-resolves
    /// what it drops: [`file_tree_refresh_roots`](Tty7App::file_tree_refresh_roots)
    /// does, and it only runs from a paint. So a caller that has cleared this
    /// cache still owes the window a frame, whatever else its batch did or did
    /// not reach (issue #243).
    fn invalidate_repo_roots(&mut self) -> bool {
        let had = !self.repo_roots.is_empty() || !self.repo_root_loads.is_empty();
        self.repo_roots.clear();
        self.repo_root_loads.invalidate_all();
        had
    }

    /// Show `op`'s result in `dir`'s cached listing before the host has
    /// confirmed it, returning the listing as it was so a failure can put it
    /// back verbatim.
    ///
    /// Optimism is the whole point: a row that only appears once the write has
    /// landed reads, on a remote host, as a keystroke the app dropped. The
    /// snapshot is what makes it safe — one `Vec` clone, and undoing is
    /// restoring rather than inverting each kind of edit.
    ///
    /// A directory with nothing cached is left alone: there is no listing to
    /// show the guess in, and inventing one would claim the directory holds
    /// only this entry.
    fn optimistic(
        &mut self,
        host: HostId,
        dir: &Path,
        op: &TreeWrite,
        target: &TreeEntry,
    ) -> Option<Vec<TreeEntry>> {
        // A listing requested before the write would otherwise land mid-edit
        // and erase the optimistic row — a visible flicker on any host whose
        // `read_dir` is not instant.
        self.loads.invalidate(&(host, dir.to_path_buf()));
        optimistic_write(&mut self.children, host, dir, op, target)
    }

    /// Put `dir`'s listing back after an [`optimistic`](Self::optimistic) guess
    /// the host rejected.
    fn rollback(&mut self, host: HostId, dir: &Path, before: Option<Vec<TreeEntry>>) {
        rollback_write(&mut self.children, host, dir, before)
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

/// The cache mutation behind [`FileTreeState::optimistic`], over the map rather
/// than the state, so it can be exercised without a GPUI app to mint a
/// `FocusHandle` from.
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

/// The undo half of [`optimistic_write`]: drop the guess and let the directory
/// relist.
///
/// Deliberately *not* "put the snapshot back". Between the guess and the
/// failure, a watcher event may have invalidated the listing and a fresh load
/// may have installed the true one — and reinstating a snapshot on top of that
/// leaves the tree showing pre-change content indefinitely, since nothing is
/// left in flight or marked stale to correct it. Discarding cannot be wrong:
/// the host is the authority, the next paint asks it, and the row vanishing on
/// failure is exactly what an optimistic write costs.
///
/// `before` is taken by value so the snapshot is consumed rather than left
/// lying around for a caller to misuse.
fn rollback_write(
    children: &mut ByHost<PathBuf, Vec<TreeEntry>>,
    host: HostId,
    dir: &Path,
    before: Option<Vec<TreeEntry>>,
) {
    drop(before);
    children.remove(host, &dir.to_path_buf());
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
    /// The machine the file tree and the code editor act on: this window's,
    /// [`spawn_host`](Tty7App::spawn_host) resolved to its host object.
    ///
    /// **Derived from the window's workspace, never cached.** A window shows one
    /// workspace and a workspace names one machine, so there is a
    /// right answer at every instant and no event to subscribe to — a tab
    /// switch, a new split, a rebind and a session restore all move it by
    /// construction. An earlier version of this re-derived the id from the
    /// active tab's panes inside
    /// [`file_tree_refresh_roots`](Self::file_tree_refresh_roots), which is only
    /// called when the root set is empty or a tab switch finds the panel open —
    /// so the tree could keep acting on the machine it was last rooted for.
    ///
    /// `None` means that machine is not reachable: a remote workspace whose
    /// connection dropped. Call sites stop there rather than falling back to the
    /// local host, which would list *this* machine's `/home/me/proj` in a tree
    /// labelled with the remote's.
    pub(crate) fn active_host(&self, cx: &App) -> Option<SharedHost> {
        HostRegistry::lookup(cx, self.spawn_host(cx))
    }

    /// Derive the root set from the active tab's panes: each pane cwd maps to
    /// its repo root (or itself outside a repo); home as the last resort.
    ///
    /// Called on every paint of the tree, so it is written to be cheap and to
    /// act only when the derived set actually differs. Deriving it once and
    /// pinning it — what this used to do — left the tree on the directory the
    /// panel happened to open in, which on a remote workspace is `$HOME` every
    /// time.
    pub(crate) fn file_tree_refresh_roots(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let id = self.spawn_host(cx);
        let Some(host) = self.active_host(cx) else {
            return; // that machine is gone; the tree keeps what it has
        };
        let leaves = match self.tabs.get(self.active) {
            Some(tab) => tab.pane.terminals(),
            None => Vec::new(),
        };
        // Only panes on the window's own machine contribute. They all are, by
        // design — but a stray one (a tab carried across a rebind) would put
        // a path from another machine into the root set, and every listing of it
        // would then be asked of the wrong host.
        //
        // `cwd`, not `host_cwd`: a native-SSH pane reports a path this machine
        // cannot answer for, and the tree has always shown it anyway (as a root
        // that lists nothing). Unchanged here on purpose — remote *workspaces*
        // are what this is fixing.
        let cwds: Vec<PathBuf> = leaves
            .iter()
            .filter(|leaf| leaf.read(cx).host_id() == id)
            .filter_map(|leaf| leaf.read(cx).cwd())
            .collect();
        // Resolving a cwd to its repository root is a host call — a walk up the
        // ancestors testing for `.git`, which is one round trip remotely. So it
        // is answered from cache, and a miss queues the resolution instead of
        // blocking the frame.
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
        // A partial answer would make the tree flap: roots would appear one
        // resolution at a time, each one clearing the caches. Every landing
        // notifies, so the next paint re-enters here and eventually completes.
        if !resolved {
            return;
        }
        // Last resort when no pane has reported a cwd yet. **Only on the local
        // host**: `HOME` is this machine's, and handing `/Users/me` to a remote
        // workspace's tree roots it at a path that does not exist over there —
        // a row that lists nothing, labelled as if it were the remote's. A
        // remote tree with nothing to root on is better left empty until the
        // first OSC 7 arrives, which is a moment away.
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
        // Dropping the caches is only correct work when the root set actually
        // moved, and doing it unconditionally would spin: this runs on every
        // paint, so a tab that can't produce any roots (no panes, no `HOME`)
        // would clear the caches and re-notify every frame.
        if roots != code.roots {
            code.roots = roots;
            // Refresh listings but keep expansion state; the caches are shared
            // (path-keyed), so a stale entry only costs a relist.
            self.file_tree.invalidate_all();
            cx.notify();
        }
        self.file_tree_sync_watch(host, cx);
    }

    /// Point the watch at every tab's roots *and* expanded directories.
    ///
    /// Two changes from the recursive watch this replaces. The set now includes
    /// expanded directories, because a non-recursive watch on the roots alone
    /// would never report a change two levels down. And it is the union across
    /// tabs, which can move while the active tab's roots stay put — closing a
    /// tab takes its roots out of it — so the comparison is against the union
    /// itself rather than riding on the per-tab check.
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

    /// Resolve one pane cwd to its repository root, unless that is already
    /// cached or already out.
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
                // Outside any repository the cwd is its own root — which is
                // also what a failed probe falls back to, so an unreadable
                // directory still shows as a root rather than vanishing.
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

    /// Whether the tree column is actually being drawn, for the work that only
    /// a visible tree can have a result for.
    ///
    /// Three conditions, because the Files tab does not always draw the local
    /// tree. An open panel on that tab reaches
    /// [`render_panel_files`](Self::render_panel_files), which hands the column
    /// to the SFTP browser instead whenever the tab's detail pane is a connected
    /// native-SSH one — the local tree is not drawn at all then, and re-reading
    /// its listings is the same waste as re-reading them for a closed panel.
    /// `open_pane_id` is the fact that branch leaves behind, so reading it here
    /// is reading the decision itself rather than re-deriving it (which would
    /// need a `Window` this callback has not got).
    ///
    /// It lags by one paint: the batch arriving between switching to such a pane
    /// and the paint that opens the browser still counts as on screen. That
    /// direction is the safe one — a single extra re-read, versus a tree that
    /// stops refreshing while somebody is looking at it.
    ///
    /// Deliberately not consulting the tab's `code.visible`: the tree left the
    /// code overlay for the panel, and the overlay draws the editor only (see
    /// `render_code_overlay`).
    pub(crate) fn file_tree_on_screen(&self, cx: &App) -> bool {
        self.right_panel_open(cx)
            && self.right_panel_tab == RightPanelTab::Files
            && self.sftp_panel.open_pane_id.is_none()
    }

    /// The Files search box's query, trimmed and lowercased. The one place it
    /// is derived, because what the tree draws turns on it in two places.
    fn file_tree_query(&self, cx: &App) -> String {
        self.file_search.read(cx).value().trim().to_lowercase()
    }

    /// Whether the Files search box has a query in it, which is the tree's
    /// other mode: [`render_file_tree_rows`](Self::render_file_tree_rows) draws
    /// [`search_rows`](FileTreeState::search_rows) then — flat hits from their
    /// own host-side walk — and the cached directory listings are not on screen
    /// at all.
    ///
    /// The test itself rather than each caller's own copy of it, because the
    /// paint and the watcher have to agree about which mode the tree is in.
    pub(crate) fn file_tree_searching(&self, cx: &App) -> bool {
        !self.file_tree_query(cx).is_empty()
    }

    /// Whether the tree is drawing directory listings, for the work that only a
    /// drawn listing can have a result for — which is every `read_dir` the tree
    /// asks for.
    ///
    /// Strictly narrower than [`file_tree_on_screen`](Self::file_tree_on_screen):
    /// a searching tree *is* on screen, and still owes its column paints, but
    /// none of them read a listing. Relisting for one is the same waste as
    /// relisting for a closed panel — a round trip per marked directory per
    /// event batch, on a remote workspace — and the marks carry the change
    /// across the same way, re-read by the first paint after the box is
    /// cleared.
    pub(crate) fn file_tree_listings_on_screen(&self, cx: &App) -> bool {
        self.file_tree_on_screen(cx) && !self.file_tree_searching(cx)
    }

    /// Watcher callback (debounced): mark the affected listings for a refresh
    /// and re-read them. A `.gitignore` change resets ignore state wholesale —
    /// its patterns can affect any depth below it.
    ///
    /// See [`event_can_change_a_row`] for what is deliberately ignored here.
    ///
    /// Notably this no longer repaints for its own sake (issue #243). The
    /// subscription is non-recursive over the roots plus the expanded
    /// directories — see [`sync_watch`](FileTreeState::sync_watch) — so what
    /// arrives is a change in a directory the tree is *displaying*, and a file's
    /// contents being rewritten arrives exactly as loudly as an entry appearing
    /// or disappearing. An editor saving on every keystroke, a build writing its
    /// log next to the sources, a formatter rewriting the file in place: each of
    /// those was a full-window redraw every [`REFRESH_DEBOUNCE`], the tree
    /// asking to be drawn before it knew whether it had anything new to draw.
    /// The re-read's own landing repaints instead, and only when the listing
    /// came back different.
    ///
    /// Two things still repaint on the event itself: the whole-cache
    /// `.gitignore` branch, whose re-walk only a paint performs, and a batch
    /// that moved a repository root. Both are gated on
    /// [`file_tree_on_screen`](Self::file_tree_on_screen), because both exist to
    /// get a paint that would do nothing while the column is not drawn — and a
    /// searching tree does want both, its walk being what `invalidate_all`
    /// restarts.
    ///
    /// The re-read is gated on the narrower
    /// [`file_tree_listings_on_screen`](Self::file_tree_listings_on_screen)
    /// instead: it is the only effect here that a search box with something in
    /// it can have no use for.
    pub(crate) fn file_tree_apply_fs_events(
        &mut self,
        host: HostId,
        paths: &HashSet<PathBuf>,
        cx: &mut Context<Self>,
    ) {
        // Every batch, named. A watcher that reports a change nobody made is
        // indistinguishable from a real edit at this layer — it just drops the
        // listing cache and the next paint relists, which over a remote link is
        // a round trip per batch. If that ever runs away, this is the line that
        // says which paths are doing it.
        log::debug!(
            target: "tty7::file_tree",
            "fs events on host {host:?}: {:?}",
            paths.iter().take(8).collect::<Vec<_>>()
        );
        // Whether anything below can act on this batch at all. The tree draws in
        // one place — the right panel's Files tab — and every effect this
        // function has is either a paint of that column or a listing read to
        // fill it. With the panel closed the subscription stays open (it is
        // derived from every tab's roots and expansion, which outlive the
        // panel), so without this check a build writing into a watched directory
        // buys a `read_dir` per marked directory, per batch, for a column nobody
        // can see — a network round trip each, on a remote workspace. The marks
        // are what carry the change across: they stay, and the first paint after
        // the panel comes back re-reads them.
        let on_screen = self.file_tree_on_screen(cx);
        // The narrower of the two, for the re-read alone: a tree with a query in
        // its search box is drawn, and owes the paints below, but draws hits
        // rather than listings — so a `read_dir` issued for it lands in a cache
        // nothing is reading. Same predicate the paint decides its own mode
        // with, rather than a second copy of the test.
        let listings_on_screen = self.file_tree_listings_on_screen(cx);
        // A `.git` coming or going moves a repository root, which is the one
        // thing the cached root derivation cannot notice by itself.
        let mut roots_moved = false;
        if paths.iter().any(|p| {
            p.file_name().is_some_and(|n| n == ".git")
                || p.parent()
                    .and_then(Path::file_name)
                    .is_some_and(|n| n == ".git")
        }) {
            roots_moved = self.file_tree.invalidate_repo_roots();
        }
        // The caches are shared across tabs, so invalidate unconditionally —
        // a hidden tab's stale listing would otherwise survive until reopened.
        let gitignore_touched = paths
            .iter()
            .any(|p| p.file_name().is_some_and(|n| n == ".gitignore"));
        if gitignore_touched && self.file_tree.gitignore_reaches_tree(host, paths) {
            // The host has already dropped the compiled matchers from inside
            // its own watcher; what is left for us is the listings that carry
            // the `ignored` flags those matchers produced.
            self.file_tree.invalidate_all();
            // Repaints on the event itself: `invalidate_all` restarts the
            // search, and only a paint re-walks it. `SearchState::restart`
            // clears `pending`, so a panel that is closed here re-walks on the
            // paint that reopens it instead.
            if on_screen {
                cx.notify();
            }
        } else {
            let mut touched = false;
            for dir in dirs_to_relist(paths, self.file_tree.show_hidden) {
                touched |= self.file_tree.invalidate_dir(host, dir);
            }
            // A batch that moved a repository root owes the window a frame
            // whatever else it reached: `file_tree_refresh_roots` is what
            // re-resolves the cache just cleared, and it only runs from a paint.
            // Nothing else here would ask for one — `.git` is a dot-file, so
            // under the default `show_hidden: false` it never survives
            // `dirs_to_relist`, and a batch naming nothing but `.git` leaves
            // `touched` false and returns just below.
            if roots_moved && on_screen {
                cx.notify();
            }
            if !touched {
                // Nothing the tree holds was reached, so there is nothing to
                // re-read and nothing more to redraw.
                return;
            }
            // Deliberately *not* restarting the search here. Its results are
            // their own walk rather than a view over the listings just dropped,
            // so they do go stale — but restarting on every event batch starves
            // the walk outright: this callback fires roughly every
            // `REFRESH_DEBOUNCE`, and a restart bumps the generation that the
            // walk re-checks after waiting `SEARCH_DEBOUNCE`, so under any
            // sustained churn (a build writing into a directory somebody has
            // expanded — the watch follows the expansion set and knows nothing
            // about gitignore) every
            // walk bows out before it ever reads a directory and the list stays
            // empty forever. A snapshot that's a few seconds old until the next
            // keystroke is the better failure.

            // Re-read what was marked, here rather than on the next paint.
            // Waiting for one would mean notifying to *get* one, which is the
            // full-window redraw per event batch this function's doc comment is
            // about. Only while the listings are the thing being drawn, and only
            // while the tree is still pointed at the machine the events came
            // from.
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
        let host = self.spawn_host(cx);
        let Some(code) = self.tab_code() else {
            return;
        };
        let rows = self
            .file_tree
            .visible_rows(host, &code.roots, &code.expanded);
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

    /// `target_is_dir` comes from the row that opened the menu. It used to be a
    /// `Path::is_dir()` call right here — a stat on the UI thread, and on a
    /// remote host a round trip before the input box could even appear.
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
        // What to do, and what the row should look like while it is happening.
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
                // `Host::join` on the parent, not `Path::with_file_name`:
                // the latter re-pushes with the *client's* separator, so a
                // Windows client renaming a remote `/home/me/a.rs` would ask
                // for `/home/me\b.rs`.
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
        HostOps::run_in(
            host,
            window,
            cx,
            move |h| match &op {
                TreeWrite::NewFile => h.create_file_new(&target),
                TreeWrite::NewFolder => h.create_dir(&target, false),
                // No `exists` probe first: that is a second round trip and a
                // TOCTOU window. `Host::rename` promises `AlreadyExists`.
                TreeWrite::Rename { from } => h.rename(from, &target),
                TreeWrite::Delete => h.remove(&target, is_dir),
            },
            move |app, result: std::io::Result<()>, window, cx| {
                match result {
                    Ok(()) => {
                        // Relist for the truth — the optimistic row guessed at
                        // `ignored`, and the host is the authority on ordering.
                        app.file_tree.invalidate_dir(id, &dir);
                        // A freshly created file opens straight into the editor.
                        if matches!(edit, TreeEdit::NewFile { .. }) {
                            app.open_file_in_editor(&new_path, window, cx);
                        }
                    }
                    Err(e) => {
                        // Put the row back and say why. Leaving it would show a
                        // file that does not exist until something else
                        // happened to relist the directory.
                        app.file_tree.rollback(id, &dir, rollback);
                        if let Some(code) = app.tab_code_mut()
                            && code.selected.as_deref() == Some(&*new_path)
                        {
                            code.selected = None;
                        }
                        use gpui_component::WindowExt as _;
                        window.push_notification(format!("{e}"), cx);
                    }
                }
                cx.notify();
            },
        );
        cx.notify();
    }

    /// Context-menu delete, with a native confirm (recursive for dirs).
    ///
    /// `is_dir` comes from the row rather than from a `stat`: the tree already
    /// knows, and asking the host would put a round trip between the click and
    /// the confirmation dialog.
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
            "The folder and everything inside it will be deleted."
        } else {
            "The file will be deleted."
        };
        let answer = window.prompt(
            PromptLevel::Warning,
            &format!("Delete \"{name}\"?"),
            Some(detail),
            // Safe option first: the leading button is the Return-key default on
            // macOS (NSAlert) and Windows (TaskDialog); "Cancel" is what gpui maps
            // to the Escape key.
            &["Cancel", "Delete"],
            cx,
        );
        cx.spawn_in(window, async move |app, cx| {
            let Ok(1) = answer.await else { return };
            let _ = app.update_in(cx, |app, window, cx| {
                let Some(host) = app.active_host(cx) else {
                    return;
                };
                let id = host.id();
                let Some(parent) = path.parent().map(Path::to_path_buf) else {
                    return;
                };
                // Optimistic: the row goes now, not a round trip later.
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
                                HostOps::notify_err(window, cx, "Delete failed", &e);
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

/// A committed inline edit, reduced to the host call it becomes. Carries the
/// rename's source because that is the one piece the destination path does not
/// already imply.
enum TreeWrite {
    NewFile,
    NewFolder,
    Rename { from: PathBuf },
    Delete,
}

// ---------------------------------------------------------------------------
// Rendering.
// ---------------------------------------------------------------------------

impl Tty7App {
    /// The file-tree column: just the scrolling rows of the tree — no header, no
    /// fixed width, no surface of its own — because the one thing that draws it
    /// already has those. That is the right panel's Files tab, and only it: the
    /// tree left the code overlay, which now draws the editor alone.
    ///
    /// The single draw site is why [`file_tree_on_screen`](Self::file_tree_on_screen)
    /// is three conditions and no more, and why everything this function does
    /// per paint — re-rooting, moving the watched set, requesting listings —
    /// stops happening the moment the panel closes.
    ///
    /// Requesting listings stops a step earlier than the rest: a query in the
    /// search box puts the column in its other mode, drawing hits from their own
    /// walk, and no cached listing is read at all. That test is
    /// [`file_tree_searching`](Self::file_tree_searching) rather than a local
    /// one, because the watcher has to reach the same answer — see
    /// [`file_tree_apply_fs_events`](Self::file_tree_apply_fs_events).
    pub(crate) fn render_file_tree_rows(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        // Re-derive every paint, not just when the tree has no roots yet.
        //
        // The root of a pane is its **repository** root, so this does not make
        // the tree chase the shell: `cd` inside a project resolves to the same
        // root and nothing moves. What does move it is `cd`-ing to another
        // project — which is exactly when the tree showing the old one is
        // wrong. Rooting only on "empty, tab switch, panel toggle" pinned a
        // remote workspace's tree to `$HOME` forever, because that is where a
        // fresh login sits when the panel first opens.
        //
        // `file_tree_refresh_roots` compares before it acts: an unchanged root
        // set costs a few map lookups and touches nothing.
        self.file_tree_refresh_roots(window, cx);
        let (roots, expanded) = match self.tab_code() {
            Some(code) => (code.roots.clone(), code.expanded.clone()),
            None => (Vec::new(), std::collections::HashSet::new()),
        };
        let query = self.file_tree_query(cx);
        // `None` — the tree's machine has gone away — still renders: the rows
        // come from caches keyed by its id, so the tree stays on screen as it
        // last was instead of blanking. What stops is the work that needs the
        // machine: no watch to move, no listings to request.
        let host = self.active_host(cx);
        let host_id = self.spawn_host(cx);
        // The watched set follows the expanded set, and expansion is toggled
        // from half a dozen places (click, arrow keys, reveal, a new inline
        // edit). Syncing here instead means one place that cannot be forgotten;
        // the steady-state cost is a set comparison over a few dozen paths.
        if let Some(host) = host.clone() {
            self.file_tree_sync_watch(host, cx);
        }
        // Both branches only read caches; whatever is missing is queued onto the
        // background executor and shows up on the paint after it lands.
        self.file_tree.sync_search(&query, &roots, cx);
        let rows = if self.file_tree_searching(cx) {
            self.file_tree.search_rows()
        } else {
            if let Some(host) = &host {
                self.file_tree.request_loads(host, &roots, &expanded, cx);
            }
            self.file_tree.visible_rows(host_id, &roots, &expanded)
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
                        this.file_tree_begin_edit(TreeEditKind::NewFile, &p, is_dir, window, cx)
                    });
                }
            }))
            .item(PopupMenuItem::new("New Folder").on_click({
                let app = app.clone();
                let p = p.clone();
                move |_, window, cx| {
                    let _ = app.update(cx, |this, cx| {
                        this.file_tree_begin_edit(TreeEditKind::NewFolder, &p, is_dir, window, cx)
                    });
                }
            }));

        if !is_root {
            menu = menu.item(PopupMenuItem::new("Rename").on_click({
                let app = app.clone();
                let p = p.clone();
                move |_, window, cx| {
                    let _ = app.update(cx, |this, cx| {
                        this.file_tree_begin_edit(TreeEditKind::Rename, &p, is_dir, window, cx)
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
            .item(
                PopupMenuItem::new(crate::ui::right_panel::reveal_label()).on_click({
                    let p = p.clone();
                    move |_, _window, cx| {
                        cx.reveal_path(&p);
                    }
                }),
            );

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
                        let _ =
                            app.update(cx, |this, cx| this.file_tree_delete(p, is_dir, window, cx));
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

/// The body of [`FileTreeState::land_load`], over the two fields it touches so
/// the rule can be tested without an `App` to hang a whole tree state off.
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
    // What was on screen has just been replaced, so the mark goes. A listing
    // superseded in flight is re-requested by the caller rather than by the
    // mark, which is why clearing it here cannot lose the refresh.
    stale.remove(key);
    superseded
}

/// The directories a batch of watcher events can have changed the listing of.
///
/// The parent of each event path, and **not the path itself**. A row is a child
/// of the directory it appears under, so an event on `d` is news for `d`'s
/// parent; `d`'s own listing changes only when something inside it does, and
/// that arrives as an event on that child.
///
/// This is not just an economy. A watched directory gets an event of its own
/// whenever anything inside it is touched — including the dot-files
/// [`event_can_change_a_row`] deliberately skips — so relisting `d` for `d`'s
/// event puts back exactly the round trip that filter exists to avoid. `$HOME`
/// with a coding agent rewriting `~/.claude.json` was one relist of the home
/// directory per write, forever.
fn dirs_to_relist(paths: &HashSet<PathBuf>, show_hidden: bool) -> HashSet<&Path> {
    paths
        .iter()
        .filter(|p| event_can_change_a_row(p, show_hidden))
        .filter_map(|p| p.parent())
        .collect()
}

/// Whether a watcher event for `path` can change a row the tree is showing.
///
/// A dot-file that is not on screen cannot, so relisting for it buys nothing
/// and costs a round trip. Worth skipping rather than merely wasteful: `$HOME`
/// on a machine somebody works on holds several files rewritten continuously
/// (`.claude.json`, `.bash_history`, shell state), and `$HOME` is exactly where
/// a fresh remote workspace roots its tree — so the relisting never stops.
///
/// `show_hidden` is consulted rather than assumed: with hidden entries on
/// screen these events matter again. `.git` and `.gitignore` are handled by
/// their own tests *before* this one — those are dot-files whose contents
/// change what the visible rows mean, which is a different question from
/// whether the file itself is a row.
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

    /// A listing that was superseded while in flight still lands.
    ///
    /// Dropping it starves a directory that changes faster than the round trip:
    /// every answer arrives stale, so the cache stays empty and the rows blink
    /// out on every paint. That is unreachable locally (microseconds) and
    /// routine over SSH, which is why it survived until a remote workspace hit
    /// it — one file rewritten a few times a second was enough.
    #[test]
    fn a_listing_superseded_in_flight_is_still_shown() {
        let mut loads: InFlight<DirKey> = InFlight::default();
        let mut children: ByHost<PathBuf, Vec<TreeEntry>> = ByHost::default();
        let id = HostId::LOCAL;
        let dir = PathBuf::from("/home/me");
        let key: DirKey = (id, dir.clone());

        let mut stale: HashSet<DirKey> = HashSet::new();

        assert!(loads.begin(key.clone()), "the listing goes out");
        // A watcher event lands mid-flight — the case that used to discard.
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

        // The undisturbed case still reports "no need to go round again".
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

    /// An outdated listing keeps its rows on screen until the replacement
    /// lands, and the replacement clears the mark.
    ///
    /// Dropping it at invalidation time is invisible locally and strobes over a
    /// link: every watcher batch blanks the directory for a whole round trip.
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

        // What `invalidate_dir` does to a cached listing: mark, never remove.
        stale.insert(key.clone());
        assert_eq!(
            children.get(id, &dir).map(Vec::len),
            Some(1),
            "the rows are still there to paint"
        );

        // …and the refresh does go out, which is what `request_load` asks.
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

    /// A directory's own watcher event relists its *parent*, not itself.
    ///
    /// Relisting itself hands back the round trip that skipping dot-files
    /// saves: a watched directory gets an event of its own for every write
    /// inside it, hidden or not.
    #[test]
    fn a_directorys_own_event_does_not_relist_it() {
        // One `~/.claude.json` write, as the watcher reports it.
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

        // A visible file appearing under it does relist it — via its own path.
        let batch: HashSet<PathBuf> = [
            PathBuf::from("/home/me/notes.md"),
            PathBuf::from("/home/me"),
        ]
        .into_iter()
        .collect();
        assert!(dirs_to_relist(&batch, false).contains(Path::new("/home/me")));

        // And with hidden entries shown, the dot-file is a row again.
        let batch: HashSet<PathBuf> = [PathBuf::from("/home/me/.claude.json")]
            .into_iter()
            .collect();
        assert!(dirs_to_relist(&batch, true).contains(Path::new("/home/me")));
        assert!(dirs_to_relist(&batch, false).is_empty());
    }

    /// The churn that made a remote tree flicker: a coding agent rewriting
    /// `~/.claude.json` several times a second, under a tree rooted at `$HOME`.
    /// The file is not a row (hidden), so it must not cost a listing — and must
    /// start costing one the moment hidden entries are shown.
    #[test]
    fn an_unshown_dot_file_does_not_trigger_a_relist() {
        let hidden = Path::new("/home/me/.claude.json");
        let visible = Path::new("/home/me/src");
        assert!(!event_can_change_a_row(hidden, false));
        assert!(event_can_change_a_row(hidden, true));
        assert!(event_can_change_a_row(visible, false));
        // A dot-*directory* is a row too once hidden entries are shown.
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

    /// The listing the tree builds out of `Host::read_dir`, and the hits it
    /// builds out of `Host::search`, still mean what the tree's own walk meant:
    /// deepest gitignore match wins, `!` un-ignores, `.git` is ignored whatever
    /// the patterns say, and ignored entries stay out of the search.
    ///
    /// The walk itself now lives in the host — this pins the *call*, which is
    /// the part that is ours to get wrong: the two budgets and `show_hidden`
    /// have to reach the host or the search silently changes shape.
    #[test]
    fn the_tree_reads_the_same_listing_out_of_the_host() {
        let host = tty7_core::host::local::LocalHost::new();
        // The fixture is built through the host too. Partly because it is the
        // thing under test and partly because it keeps this module honest: the
        // CI grep that forbids direct filesystem calls in `src/ui` does not
        // know test modules from production code, and it should not have to.
        let tmp = std::env::temp_dir().join(format!("tty7-tree-host-{}", std::process::id()));
        let _ = host.remove(&tmp, true);
        host.create_dir(&tmp.join(".git"), true).unwrap();
        host.create_dir(&tmp.join("src"), true).unwrap();
        host.write_file(&tmp.join(".gitignore"), b"*.log\nbuild/\n")
            .unwrap();
        // The deeper file un-ignores one of the parent's patterns.
        host.write_file(&tmp.join("src/.gitignore"), b"!keep.log\n")
            .unwrap();
        host.write_file(&tmp.join("drop.log"), b"").unwrap();
        host.write_file(&tmp.join("src/keep.log"), b"").unwrap();
        host.write_file(&tmp.join("src/main.rs"), b"").unwrap();

        // The exact mapping `request_load` performs, `Host::join` included.
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

        // Showing dotfiles lets the ignored ones back in — the flag has to
        // reach the host, because the walk is where the filtering happens.
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

    /// `Host::read_dir` follows symlinks, so a link to a directory is now an
    /// expandable directory — which means the search walk can follow a cycle.
    ///
    /// There is no cycle detection anywhere in the tree, and this pins why none
    /// is needed for termination: `SEARCH_MAX_DIRS` bounds the walk. What it
    /// does not prevent is a cycle near a root eating the whole budget, so the
    /// test also shows the walk still finds a real hit past the loop.
    #[cfg(unix)]
    #[test]
    fn a_symlink_cycle_cannot_make_the_search_walk_forever() {
        let host = tty7_core::host::local::LocalHost::new();
        let tmp = std::env::temp_dir().join(format!("tty7-tree-loop-{}", std::process::id()));
        let _ = host.remove(&tmp, true);
        host.create_dir(&tmp, true).unwrap();
        host.write_file(&tmp.join("needle.rs"), b"").unwrap();
        // `a/loop -> a`: every listing of it yields another directory to visit.
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

        // And the link itself reads as a directory — the behaviour change that
        // makes the cycle reachable in the first place.
        let listed = host.read_dir(&tmp.join("a"), Some(&tmp)).unwrap();
        let link = listed.iter().find(|e| e.name == "loop").expect("link");
        assert!(link.is_dir, "a link to a directory expands as one");
        assert!(link.is_symlink);

        let _ = host.remove(&tmp, true);
    }

    /// M2 regression guard: a create, a rename and a delete
    /// each show their result before the host has confirmed it, and a failure
    /// leaves the directory to relist rather than showing a row for a file that
    /// does not exist.
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

        // Create: the new row appears, sorted into place.
        seed(&mut children);
        let new = entry("a.rs", false);
        let before = optimistic_write(&mut children, host, &dir, &TreeWrite::NewFile, &new);
        assert!(before.is_some());
        assert_eq!(names(&children), vec!["a.rs", "b.rs"]);

        // Rename: the old row goes, the new one arrives.
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

        // Delete: the row goes immediately.
        seed(&mut children);
        let doomed = entry("b.rs", false);
        optimistic_write(&mut children, host, &dir, &TreeWrite::Delete, &doomed);
        assert!(names(&children).is_empty());

        // A rejected write discards the listing entirely, so the next paint
        // asks the host instead of trusting either the guess or a snapshot.
        seed(&mut children);
        let before = optimistic_write(&mut children, host, &dir, &TreeWrite::NewFile, &new);
        rollback_write(&mut children, host, &dir, before);
        assert!(
            children.get(host, &dir).is_none(),
            "a failed write leaves the directory to relist"
        );

        // The case that motivates discarding rather than restoring: a relist
        // landed while the write was in flight, so the cache already holds the
        // truth. Putting a pre-change snapshot back over it would stick.
        seed(&mut children);
        let before = optimistic_write(&mut children, host, &dir, &TreeWrite::NewFile, &new);
        children.insert(host, dir.clone(), vec![entry("fresh.rs", false)]);
        rollback_write(&mut children, host, &dir, before);
        assert!(
            children.get(host, &dir).is_none(),
            "the stale snapshot never overwrites a newer listing"
        );

        // A directory nobody has listed stays unlisted rather than being
        // invented as a one-entry listing.
        let other = PathBuf::from("/y");
        let before = optimistic_write(&mut children, host, &other, &TreeWrite::NewFile, &new);
        assert!(before.is_none());
        assert!(children.get(host, &other).is_none());
        rollback_write(&mut children, host, &other, before);
        assert!(children.get(host, &other).is_none());
    }
}

/// Issue #243 at the window: what a watcher event costs in frames.
///
/// The other half of that issue — a refreshing directory keeping its rows on
/// screen — is [`FileTreeState::stale`]'s job and is covered by
/// `land_listing_keeps_a_superseded_snapshot`. These are about the frames: a
/// window with nothing new to draw must not be asked to draw.
///
/// Measurements, not assertions about internals. gpui's test build redraws every
/// dirty window from inside `flush_effects`, so a real (headless) window plus
/// [`render_probe`](crate::ui::app::render_probe) counts exactly the frames the
/// app asked for — the one claim in the issue that is platform-independent, and
/// answerable without the reporter's Wayland session.
///
/// What the live watch can actually deliver bounds what any of this can claim.
/// The subscription is non-recursive over the roots plus the expanded
/// directories, so the reachable case is a change *in a directory on screen* —
/// that is the one measured. Three cases here feed [`fs_event`] a path the live
/// subscription would drop before batching it; each is labelled a guard on the
/// predicate it exercises, and none of them is evidence of a symptom.
#[cfg(all(test, unix))]
mod render_idle_gpui_tests {
    use super::*;
    use crate::daemon::protocol::DaemonMsg;
    use crate::ui::app::{render_probe, test_window};
    use gpui::{Entity, TestAppContext, VisualTestContext};
    use tty7_core::core::config::RightPanelTab;

    /// Draws a settle may legitimately spend before the count reads as a loop.
    /// A repaint loop blows past this in well under a second, and it fails the
    /// test rather than hanging it — the loop lives inside one `flush_effects`
    /// call, which nothing outside it can interrupt.
    const BUDGET: u64 = 200;

    /// These run one at a time. Every one of them drives a real window through
    /// `LocalHost::shared()`, which is a process-wide `OnceLock` singleton with
    /// a shared gitignore cache and its own pool — so concurrent cases contend
    /// on it and the draw counts, which are the whole point here, stop being
    /// meaningful. Poisoning is stepped over deliberately: one failing case
    /// should report its own assertion, not turn every later one into a panic
    /// about a poisoned lock.
    fn serial() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("tty7-idle-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // macOS reports watcher paths through `/private/var` while the cache is
        // keyed by the root as handed in. Canonicalizing keeps these tests about
        // the frames rather than about that.
        std::fs::canonicalize(&dir).unwrap()
    }

    /// A window with the right panel open on the Files tab, rooted at `root` and
    /// settled: the first listing has landed and everything it woke has run.
    fn files_panel_on(
        cx: &mut TestAppContext,
        root: &Path,
    ) -> (
        Entity<Tty7App>,
        VisualTestContext,
        std::os::unix::net::UnixStream,
    ) {
        let (app, mut vcx, mut pane) = test_window::harness_with_pane(cx);
        // Tell the pane where it is, rather than writing the roots directly:
        // render re-derives them from the active tab's panes every frame, so a
        // root assigned behind that is replaced by the `$HOME` fallback on the
        // very next paint. The test plays the daemon, and `Cwd` is the message a
        // shell's OSC 7 turns into.
        DaemonMsg::Cwd(root.to_path_buf())
            .encode(&mut pane)
            .expect("the pane's socket takes the cwd");
        app.update_in(&mut vcx, |app, _, cx| {
            app.right_panel_visible = true;
            app.right_panel_tab = RightPanelTab::Files;
            cx.notify();
        });
        // The pane's reader is a real thread and the cwd has to reach it, then
        // be resolved to a repository root (a host call, answered off-thread),
        // before the first listing is even asked for.
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
        // And then until the root's own listing has landed, so a measurement
        // never starts against a tree that has not drawn its rows yet.
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
        (app, vcx, pane)
    }

    /// What the tree would paint right now.
    fn rows(app: &Entity<Tty7App>, vcx: &mut VisualTestContext) -> usize {
        app.update_in(vcx, |app, _, _| {
            let code = app.tab_code().expect("panel state");
            app.file_tree
                .visible_rows(HostId::LOCAL, &code.roots, &code.expanded)
                .len()
        })
    }

    /// Hand the app one debounced batch at the seam the watcher delivers to.
    ///
    /// The seam, not the watcher: this bypasses `WatchedDirs::translate`, so a
    /// caller can synthesise a path the live subscription would never deliver.
    /// The cases that do are labelled as guards on a predicate rather than as
    /// symptoms, and say so.
    fn fs_event(app: &Entity<Tty7App>, vcx: &mut VisualTestContext, path: &Path) {
        app.update_in(vcx, |app, _, cx| {
            app.file_tree_apply_fs_events(HostId::LOCAL, &HashSet::from([path.to_path_buf()]), cx);
        });
    }

    /// Run everything the app has queued, including the host's real-thread
    /// listings, to quiescence. A single `run_until_parked` is not enough: the
    /// host answers off the deterministic executor, so its reply lands after the
    /// test thread has already parked.
    fn settle(app: &Entity<Tty7App>, vcx: &mut VisualTestContext, root: &Path) {
        // A wall-clock deadline, not an iteration count: the whole suite shares
        // one `LocalHost` pool, so under a parallel `cargo test` a listing can
        // take far longer to come back than it does alone.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while std::time::Instant::now() < deadline {
            vcx.background_executor.run_until_parked();
            // Loads, plus any mark on the tree *this test is looking at*.
            // Deliberately not `stale.is_empty()`: the whole-cache branch marks
            // every cached listing, and the cache outlives the panel's roots —
            // the `$HOME` listing from before the pane reported its cwd is still
            // in there, is not a root or an expanded directory, so
            // `request_loads` never re-asks for it and its mark never clears. A
            // settle waiting on that waits forever.
            //
            // Deliberately does not notify either — a settle that asked for
            // paints would be counted by the draw probe it exists to serve.
            let quiet = app.update_in(vcx, |app, _, _| {
                app.file_tree.loads.is_empty()
                    && !app
                        .file_tree
                        .stale
                        .iter()
                        .any(|(_, dir)| dir.starts_with(root))
            });
            if quiet {
                // One more pass so the last landing's own notify is drawn.
                vcx.background_executor.run_until_parked();
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("the tree never went quiet");
    }

    /// Draws over a quiet interval — no input, no filesystem change — *after*
    /// the window has come to rest. Anything counted here is a frame the window
    /// asked for with nothing to draw, which is what issue #243 is about.
    ///
    /// The rest comes first because settling legitimately costs a last frame or
    /// two: the final listing lands and asks to be drawn. Render idle is not
    /// "never draws again", it is "stops drawing" — so the measurement is the
    /// second interval, once the first has absorbed the tail. A repaint loop
    /// keeps both intervals busy and trips [`BUDGET`] long before either ends.
    fn draws_while_idle(vcx: &mut VisualTestContext) -> u64 {
        render_probe::arm(BUDGET);
        vcx.background_executor.run_until_parked();
        vcx.executor()
            .advance_clock(std::time::Duration::from_secs(3));
        vcx.background_executor.run_until_parked();
        // Now it is at rest. Count from here.
        render_probe::arm(BUDGET);
        vcx.executor()
            .advance_clock(std::time::Duration::from_secs(9));
        vcx.background_executor.run_until_parked();
        render_probe::draws()
    }

    /// The reporter's failing case and their two negative cases, measured the
    /// same way: a settled window draws once and stops, whatever is in the tree.
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

    /// A guard on the `!touched` early return, **not** a symptom.
    ///
    /// Read the event below for what it is: `root/target/debug/artifact0.o`,
    /// whose parent is neither a root nor an expanded directory. The live watch
    /// cannot deliver it — it is non-recursive over roots ∪ expanded, and
    /// `WatchedDirs::translate` drops anything whose parent is not in that set
    /// before it is ever batched. So this measures no bug that can occur today.
    /// What it holds down is the predicate: `invalidate_dir` answering "nothing
    /// here" must stay a silent return, because that answer is also what a
    /// watched directory with no landed listing gives, and because expanding
    /// the watched set (or restoring a recursive watch) would make this exact
    /// path live again.
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

    /// The reachable case, and the one issue #243 is actually about on this
    /// base: a directory the tree *is* showing, where what changed is a file's
    /// contents rather than the set of entries.
    ///
    /// The displayed directories are exactly the watched ones, so this event is
    /// delivered — a formatter rewriting the file, an editor saving on every
    /// keystroke, a build dropping its log next to the sources. Each one cost a
    /// full-window redraw, twice over: once for the `cx.notify()` in the event
    /// handler and once for the landing of the relist it bought.
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

    /// Not repainting for an event is only correct if a real change still
    /// arrives — the re-read now happens in the event handler rather than on a
    /// paint that a `cx.notify()` had to buy.
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

        // Deletions too, and then the window settles again.
        std::fs::remove_file(&added).unwrap();
        fs_event(&app, &mut vcx, &added);
        settle(&app, &mut vcx, &root);
        assert_eq!(rows(&app, &mut vcx), before);
        assert_eq!(draws_while_idle(&mut vcx), 0);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A guard on [`FileTreeState::gitignore_reaches_tree`], **not** a symptom.
    ///
    /// Same caveat as `an_event_reaching_no_cached_listing_costs_no_frames`:
    /// `root/node_modules/pkg0/.gitignore` sits under a directory nobody
    /// expanded, so the live non-recursive watch never delivers it and the
    /// `npm install` story this test used to tell cannot happen here. It is kept
    /// because the branch it guards is the expensive one — `invalidate_all`
    /// marks every cached listing and restarts the search — and the predicate
    /// deciding when to take it deserves a test that a refactor cannot quietly
    /// invert.
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

    /// The other side of that scoping: a `.gitignore` that *can* reach the tree
    /// still refreshes it, and the panel does not empty while it does.
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
        // Every cached listing is marked, which is what the whole-cache branch
        // is for — and the rows are still on screen while it re-reads.
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
        // Nothing appears or disappears — an ignored entry renders dimmed, not
        // hidden — and the re-read has cleared every mark.
        assert_eq!(rows(&app, &mut vcx), before);
        let left = app.update_in(&mut vcx, |app, _, _| {
            app.file_tree
                .stale
                .iter()
                .filter(|(_, dir)| dir.starts_with(&root))
                .count()
        });
        assert_eq!(left, 0, "every marked listing under the root was re-read");
        // Deliberately not asserting the `ignored` flags here. The compiled
        // matchers live in the host and are dropped from inside *its* watcher,
        // which driving `file_tree_apply_fs_events` directly bypasses — so what
        // this seam owns is the marking and the re-read, not what the host
        // recomputes. `loader_tags_ignored_entries_down_the_gitignore_chain`
        // covers the matchers themselves.
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A guard on `stale` staying bounded, **not** a symptom.
    ///
    /// The `target/debug/*.o` events below are again ones the live watch cannot
    /// deliver. What the test defends is that `stale` is keyed by path and the
    /// handler is fed paths it does not choose: marking one for a directory the
    /// tree does not hold would let that set grow without limit, and the only
    /// thing standing between it and that is `invalidate_dir` inserting solely
    /// where `children` already has an entry.
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

    /// The one batch that reaches nothing and must still draw: a `.git`
    /// appearing or disappearing in a watched directory.
    ///
    /// `invalidate_repo_roots` empties the cwd → repository-root cache, and the
    /// only thing that fills it again is `file_tree_refresh_roots`, which runs
    /// from a paint. Meanwhile `.git` is a dot-file, so it never survives
    /// `dirs_to_relist` under the default `show_hidden: false` and the batch
    /// lands on the "reached nothing" return — which, unguarded, would leave the
    /// cache cleared with nothing to resolve it and the tree showing the old
    /// root until an unrelated event bought a frame.
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

        // Come to rest first, so the frame counted below is the event's own and
        // not one still owed from settling — the same reason `draws_while_idle`
        // measures its second interval rather than its first.
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

        // The paint re-requests the root, and the host answers off the
        // deterministic executor, so this waits on wall-clock like `settle`
        // does rather than on a single `run_until_parked`.
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

    /// A closed panel asks the host for nothing.
    ///
    /// The subscription outlives the panel — `file_tree_sync_watch` derives it
    /// from every tab's roots and expansion, which persist — so events keep
    /// arriving for a column nobody can see. Re-reading for them is a `read_dir`
    /// per marked directory per batch, which on a remote workspace is that many
    /// network round trips. The marks are what carry the change across instead.
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
        // Long enough for a re-read to have been issued *and* landed. The host
        // answers off the deterministic executor, so a single `run_until_parked`
        // would leave "no work was done" and "the work has not come back yet"
        // indistinguishable; after this, an unwanted re-read shows up as the
        // mark below having cleared itself.
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

        // And it is picked up by the first paint after the panel comes back,
        // which is what a mark is for.
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

    /// The same thing one case further in: the panel is open, the tree column is
    /// drawn, and it is drawing *search hits*. Those come from their own
    /// host-side walk, so the cached listings are as invisible as they are
    /// behind a closed panel, and re-reading one for an event batch buys a round
    /// trip nothing renders.
    ///
    /// The query is set on the input rather than typed, which is the same state:
    /// both the paint and the watcher read `file_search` for the answer.
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
        // Long enough for a re-read to have been issued *and* landed, for the
        // same reason `a_closed_panel_does_no_filesystem_work` waits: an
        // unwanted one shows up as the mark below having cleared itself.
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

        // And clearing the box picks it up, exactly as reopening the panel does.
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

    /// The Files tab is open and the local tree is still not drawn: the SFTP
    /// browser has the column, because the tab's detail pane is a connected
    /// native-SSH one.
    ///
    /// Driven through `sftp_panel.open_pane_id` rather than through a real SSH
    /// pane, which this harness cannot stand up — that field *is* what
    /// `render_panel_files` branches on, set by `sftp_sync_pane` on the paint
    /// that opens the browser, so it is the state the predicate has to read.
    /// Everything happens inside one `update` because a paint would run
    /// `sftp_sync_pane` against this window's local pane and close the browser
    /// again, which is correct behaviour and would undo the setup.
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

        // The mark is picked up once the tree has the column back.
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
