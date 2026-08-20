//! Everything the source control panel remembers between frames.
//!
//! All of it is app-level, hanging off `Tty7App.scm` rather than off a tab.
//! The panel has exactly one instance per window — the same model
//! `RightPanelState.diff_cwd` already uses. `Tab.diff_overlay` and `Tab.code`
//! are per-tab because they are full-screen overlays that belong to a tab; a
//! side panel does not.
//!
//! The one thing that must survive everything is the commit draft, so it is
//! keyed by repository rather than by tab or pane: a working tree has one
//! pending message, no matter how many panes are looking at it.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use gpui::Entity;
use gpui_component::input::InputState;
use tty7_core::core::git::log::{Commit, CommitFile, CommitPage, GraphScope};
use tty7_core::core::git::status::HeadState;

use crate::ui::host_ops::HostId;

/// Which working tree a piece of state belongs to.
///
/// The host is part of the key because the same path can exist on this machine
/// and on three different remotes at once, and they are unrelated repositories.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub(crate) struct RepoKey {
    pub(crate) host: HostId,
    pub(crate) root: PathBuf,
}

/// The four sections of the file list, in the order they are rendered.
///
/// `Merge` only appears while a merge is unresolved, which is why the panel
/// asks for it by variant rather than always drawing a header.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) enum ScmGroup {
    Merge,
    Staged,
    Changes,
    Untracked,
}

impl ScmGroup {
    pub(crate) const ORDER: [ScmGroup; 4] = [
        ScmGroup::Merge,
        ScmGroup::Staged,
        ScmGroup::Changes,
        ScmGroup::Untracked,
    ];
}

#[derive(Default)]
pub(crate) struct ScmPanelState {
    /// Which repository the panel is showing. Follows the active pane unless
    /// `repo_override` says otherwise.
    pub(crate) repo: Option<RepoKey>,
    /// Set when the user picks a repository from the multi-repo dropdown, and
    /// cleared whenever the active tab changes — an explicit choice should
    /// outlive a pane switch inside one tab, not a jump to somewhere else.
    pub(crate) repo_override: Option<RepoKey>,
    /// The tab the override was made on, so the jump away can be noticed.
    pub(crate) override_tab: Option<usize>,
    /// Local branch names per repository, with the epoch they were read at.
    /// Anything that could have moved a ref bumps the epoch, so the list in
    /// the switcher is never older than the last operation.
    pub(crate) branches: HashMap<RepoKey, (u64, Vec<String>)>,
    pub(crate) branches_loading: HashSet<RepoKey>,
    /// The inline "name your branch" input, present only while it is open.
    pub(crate) new_branch: Option<Entity<InputState>>,
    /// The inline "switch to which branch" input, present only while it is
    /// open. Never open at the same time as `new_branch` — opening either one
    /// closes the other, since the panel has room for one input row.
    pub(crate) checkout_branch: Option<Entity<InputState>>,
    /// Unsent commit messages, one per working tree.
    pub(crate) drafts: HashMap<RepoKey, String>,
    /// The commit box. `None` until the panel has been rendered once: an
    /// `InputState` needs a real window to be created in, and this struct is
    /// built by `Default` alongside the rest of `Tty7App`.
    pub(crate) commit_input: Option<Entity<InputState>>,
    /// Which repository's draft the box is currently holding. A change here
    /// is what moves one draft out and the next one in.
    pub(crate) commit_repo: Option<RepoKey>,
    /// A commit that has been dispatched: the repository, what HEAD was
    /// before it, and the message it carried. Held until HEAD moves, so a
    /// commit a hook rejects leaves the message in the box.
    pub(crate) committing: Option<(RepoKey, HeadState, String)>,
    /// Whether the next commit rewrites HEAD. Armed from the commit dropdown
    /// rather than a checkbox row — 260px does not have a row to spare.
    pub(crate) amend: bool,
    /// Groups the user folded shut, and the ones whose fold state they have
    /// set at all. Both are needed: a group nobody has touched follows the
    /// default for its size (a thousand untracked files start folded), and
    /// opening one by hand has to outlast the next file landing in it.
    pub(crate) collapsed: HashSet<ScmGroup>,
    pub(crate) toggled: HashSet<ScmGroup>,
    /// Working directory → the repository root containing it, or `None` when
    /// there is none, with when the answer was given. The root is what every
    /// write runs from and what every cache is keyed by, so it is resolved
    /// once per directory and reused.
    pub(crate) roots: HashMap<(HostId, PathBuf), (std::time::Instant, Option<PathBuf>)>,
    pub(crate) root_lookups: HashSet<(HostId, PathBuf)>,
    /// When the panel last asked for a status that it did not get back.
    pub(crate) probe_attempt: HashMap<(HostId, PathBuf), std::time::Instant>,
    /// The status the last frame drew, as (cache key, `Arc` identity). The
    /// watcher compares against it so a global write that changed nothing does
    /// not ask for another frame.
    pub(crate) seen: Option<((HostId, PathBuf), usize)>,
    pub(crate) watch: Option<gpui::Subscription>,
    /// The cheap per-tab git status the panel last reacted to. A change in it
    /// means a command touched the repository and the expensive status is due
    /// another look.
    pub(crate) last_tab_status: Option<crate::terminal::git_status::GitStatus>,
    pub(crate) graph: GraphState,
    /// When set, the panel body is replaced by a single commit's detail view
    /// instead of the working tree.
    pub(crate) detail: Option<CommitDetailView>,
    pub(crate) scroll: gpui::ScrollHandle,
}

impl ScmPanelState {
    /// The repository the panel should act on: an explicit pick wins over
    /// whatever the active pane happens to be sitting in.
    pub(crate) fn active_repo(&self) -> Option<&RepoKey> {
        self.repo_override.as_ref().or(self.repo.as_ref())
    }

    /// Drop everything keyed to a host that left the registry. Without this,
    /// `roots` and friends grow one entry per directory ever visited on a
    /// link that no longer exists. Drafts survive on purpose: a reconnect
    /// brings the same repositories back, and unsent messages with them.
    pub(crate) fn forget_host(&mut self, host: HostId) {
        self.roots.retain(|(h, _), _| *h != host);
        self.root_lookups.retain(|(h, _)| *h != host);
        self.probe_attempt.retain(|(h, _), _| *h != host);
        self.branches.retain(|k, _| k.host != host);
        self.branches_loading.retain(|k| k.host != host);
        if self.repo_override.as_ref().is_some_and(|k| k.host == host) {
            self.repo_override = None;
            self.override_tab = None;
        }
    }

    /// A probe just answered "no repository" for `root`: every directory that
    /// resolved to it has to re-ask. Left cached, the panel keeps mapping the
    /// cwd to a repository that is gone and draws its Loading state forever.
    pub(crate) fn forget_root(&mut self, host: HostId, root: &std::path::Path) {
        self.roots
            .retain(|(h, _), (_, held)| !(*h == host && held.as_deref() == Some(root)));
    }

    pub(crate) fn draft(&self, repo: &RepoKey) -> &str {
        self.drafts.get(repo).map(String::as_str).unwrap_or("")
    }

    /// Whether a group renders folded. `count` decides it only for a group the
    /// user has never touched.
    pub(crate) fn group_collapsed(&self, group: ScmGroup, count: usize) -> bool {
        if self.toggled.contains(&group) {
            self.collapsed.contains(&group)
        } else {
            crate::ui::scm::panel::starts_collapsed(group, count)
        }
    }

    pub(crate) fn set_group_collapsed(&mut self, group: ScmGroup, collapsed: bool) {
        self.toggled.insert(group);
        if collapsed {
            self.collapsed.insert(group);
        } else {
            self.collapsed.remove(&group);
        }
    }
}

#[derive(Default)]
pub(crate) struct GraphState {
    /// Mirrors `Config::scm_graph_expanded`, which starts `false`: the history
    /// section unfurling on first open would make the panel look like a mess
    /// nobody asked for.
    pub(crate) expanded: bool,
    /// How many commits have been asked for so far. Paging grows this and
    /// re-runs the query rather than using `--skip`, which is O(skip) to walk
    /// and shifts under you when a ref moves between pages.
    pub(crate) requested: usize,
    pub(crate) loading: bool,
    /// The page the graph is currently drawn from, and which repository and
    /// query produced it. `Arc` because paint reads it while the next page is
    /// being laid out on a worker, and the key so a repository switch shows an
    /// empty graph rather than the previous repository's history.
    pub(crate) page: Option<Arc<CommitPage>>,
    pub(crate) page_key: Option<(RepoKey, u64, GraphScope)>,
    /// The key of a load that came back empty-handed. Without it a failing
    /// `git log` — a scope pinned to a since-deleted branch, a vanished
    /// repository — would be retried from every frame's render, one git
    /// process per notify, forever. The failure clears itself the moment the
    /// key changes: an epoch bump (any refresh), a new scope, a new repo.
    pub(crate) failed_key: Option<(RepoKey, u64, GraphScope)>,
    /// Filter box. Like `commit_input`, created on first render — and with the
    /// subscription that turns typing into a repaint. An `InputState` is its
    /// own entity; without this the box would take text the list never sees.
    pub(crate) search: Option<Entity<InputState>>,
    pub(crate) search_sub: Option<gpui::Subscription>,
    /// Which commit indices the list shows, cached per (page identity, query).
    /// The filter case-folds every subject and author; re-running that over
    /// 5000 commits on every frame while the box is open is real work, and
    /// even the unfiltered identity list is 40KB of indices a frame.
    pub(crate) filter_cache: Option<(Option<String>, usize, Arc<Vec<usize>>)>,
    /// An open "name a branch at this commit" input, and the rev it starts
    /// from. The panel's own naming row cannot serve this: it always creates
    /// at HEAD, and the whole point here is the commit under the cursor.
    pub(crate) naming: Option<(Entity<InputState>, String)>,
    /// Which refs the graph walks from. Three states rather than an
    /// `Option<branch>`: "this branch and its upstream", "one named branch",
    /// and "everything" are all reachable from the header's dropdown, and only
    /// the enum the data layer already takes can express all three.
    pub(crate) scope: GraphScope,
    /// The selected row, by full sha.
    pub(crate) selected: Option<String>,
    pub(crate) scroll: gpui::ScrollHandle,
    /// Height of the history section, and whether its divider is being
    /// dragged. Shaped like `right_panel_width` / `right_panel_dragging` so
    /// the same drag code works on the other axis.
    pub(crate) height: std::rc::Rc<std::cell::Cell<f32>>,
    pub(crate) dragging: std::rc::Rc<std::cell::Cell<bool>>,
}

/// The panel's second-level view: one commit's metadata and the files it
/// touched. A file-level diff is not shown here — that opens the full-screen
/// overlay, because 260px cannot render a diff and pretending otherwise would
/// mean inventing a third kind of container.
///
/// The two loaded halves are behind `Arc` because the panel clones this whole
/// struct once per frame — `render_panel_scm` cannot hand `render_commit_
/// detail` a borrow of `self.scm` and a `&mut self` at once — and a commit
/// that touched a thousand files would otherwise deep-copy a thousand paths
/// every time anything on the panel redrew.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct CommitDetailView {
    pub(crate) repo: RepoKey,
    pub(crate) oid: String,
    /// A read is out. Set before it is dispatched, so the render that runs in
    /// between does not ask for a second one.
    pub(crate) loading: bool,
    /// Whether a read has ever come back. With `loading` it is what stops the
    /// view asking again forever after a commit git could not resolve: the
    /// pair says "nothing is in flight and nothing is coming".
    pub(crate) loaded: bool,
    /// `None` until a read lands, and still `None` afterwards for a commit
    /// that is not in this repository.
    pub(crate) commit: Option<Arc<Commit>>,
    pub(crate) files: Option<Arc<Vec<CommitFile>>>,
    /// The file read came back with nothing — git errored, or the link
    /// dropped mid-read. Distinct from "still loading", and from an empty
    /// list: "0 files changed" for a commit whose files could not be read
    /// would be a confident lie.
    pub(crate) files_failed: bool,
    /// A long body starts folded — a merge from a bot can run to fifty lines,
    /// and the file list is what the reader came for.
    pub(crate) body_expanded: bool,
}

impl CommitDetailView {
    /// A commit the panel is about to show. `seed` is the row the graph
    /// already has in hand, where the caller came from the graph: the page
    /// carries every field a detail view needs, so handing it over is what
    /// keeps the common path from running `git show` for a commit that is
    /// literally on screen.
    pub(crate) fn new(repo: RepoKey, oid: String, seed: Option<Commit>) -> CommitDetailView {
        CommitDetailView {
            repo,
            oid,
            loading: false,
            loaded: false,
            commit: seed.map(Arc::new),
            files: None,
            files_failed: false,
            body_expanded: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(root: &str) -> RepoKey {
        RepoKey {
            host: HostId::LOCAL,
            root: PathBuf::from(root),
        }
    }

    #[test]
    fn an_explicit_repo_pick_outranks_the_active_pane() {
        let mut state = ScmPanelState::default();
        assert!(state.active_repo().is_none());
        state.repo = Some(key("/a"));
        assert_eq!(state.active_repo(), Some(&key("/a")));
        state.repo_override = Some(key("/b"));
        assert_eq!(state.active_repo(), Some(&key("/b")));
        state.repo_override = None;
        assert_eq!(state.active_repo(), Some(&key("/a")));
    }

    #[test]
    fn drafts_are_keyed_by_repository_not_by_path_alone() {
        let mut state = ScmPanelState::default();
        state.drafts.insert(key("/a"), "wip".into());
        assert_eq!(state.draft(&key("/a")), "wip");
        assert_eq!(state.draft(&key("/b")), "");
    }
}
