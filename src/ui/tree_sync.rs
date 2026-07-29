//! The write half of the client-side tree migration: every structural change a
//! window makes becomes **semantic operations** on the daemon-owned machine
//! tree, instead of a whole-layout write to a file.
//!
//! # Why a mirror-and-diff rather than ops at every call site
//!
//! `Tty7App::save_session` is already the single point every structural change
//! funnels through — twenty-odd call sites, each of which knows *that*
//! something changed but expresses it by handing over the whole tab list. This
//! module keeps that funnel: it holds, per window, a **mirror** of what the
//! daemon's tree looked like after the last acknowledged operation, and each
//! sync diffs the window's current state against it. Because consecutive syncs
//! differ by exactly one user action, the diff *recovers* that action — a
//! split diffs to one `PaneSplit`, a closed tab to one `TabClose` — without
//! twenty call sites each hand-rolling its own op sequence (and each being a
//! chance to get one wrong). Multi-step changes ("close other tabs") fall out
//! as the op sequence they are.
//!
//! The mirror is updated by running **the server's own tree surgery**
//! ([`PaneNode::split_leaf`] and friends are public for exactly this), so the
//! predicted post-state cannot drift from what the daemon will hold.
//!
//! # What happens when prediction and reality disagree
//!
//! Any failed operation — a refused edit, a dropped link — invalidates the
//! mirror instead of trying to patch around it: the queue is dropped, the tree
//! is re-pulled (`WorkspaceTree`), and the next diff against the *authoritative*
//! state re-emits exactly the edits that still matter. Reconciliation by
//! re-pull is the one recovery path, shared by every failure mode, which is
//! why none of them needs code of its own.
//!
//! # Panes that do not exist yet
//!
//! A leaf whose pane is still connecting (a fresh spawn with no daemon id) is
//! **invisible** to the tree until it lands: the daemon's leaves hold pane ids
//! and nothing else, so there is nothing to say yet. `land_pane`'s save is the
//! moment the id exists, and the diff then emits the `TabCreate` / `PaneSplit`
//! the earlier saves could not. A connecting leaf that is *re-attaching* to a
//! known pane id is representable all along.
//!
//! # One id space oddity
//!
//! Operations name the workspace by the **machine's** id. For a local window
//! that is the client's own [`WorkspaceId`]; for a remote one it is
//! `RemoteRef::workspace` — the id minted on that machine — while the client's
//! entry keeps its own id for the window registry. [`tree_workspace_id`] is the
//! one translation point.

use std::collections::{HashMap, VecDeque};
use std::io;
use std::sync::Arc;

use gpui::{App, Global};
use tty7_core::core::machine::{
    AgentFacts, Axis as TreeAxis, LayoutDelta, Machine, PaneNode, PaneRecord, PaneSeed, Side,
    Tab as TreeTab, TabId,
};
use tty7_core::daemon::control::{ControlClient, ControlRequest, ReplyOk};
use tty7_core::host::HostId;

use crate::core::session::{Session, SessionPane, SessionTab, WorkspaceId, WorkspaceStore};
use crate::ui::app::Tty7App;
use crate::ui::pane::{Pane, PaneSlot};

/// The control link to `host`'s daemon, if one is up right now.
///
/// The unification the whole design leans on: the local machine's link lives in
/// [`LocalLink`](crate::ui::local_link::LocalLink), a remote machine's in
/// [`HostLinks`](crate::ui::remote_connect::HostLinks), and
/// everything above this function stops caring which. `None` is always
/// transient (both holders have supervisors reconnecting), so callers treat it
/// as "not now": mark dirty and let the re-pull that follows reconnection
/// resend what still matters.
pub(crate) fn control_for(cx: &mut App, host: HostId) -> Option<Arc<ControlClient>> {
    if host.is_local() {
        crate::ui::local_link::LocalLink::client(cx)
    } else {
        crate::ui::remote_connect::HostLinks::get(cx, host)
            .map(|h| Arc::clone(h.client()))
            .filter(|c| c.is_connected())
    }
}

/// The machine-side id operations about this window's workspace must carry.
fn tree_workspace_id(cx: &App, client_ws: WorkspaceId) -> WorkspaceId {
    WorkspaceStore::all(cx)
        .get(client_ws)
        .and_then(|w| w.host.as_ref())
        .map(|r| r.workspace)
        .unwrap_or(client_ws)
}

// ---------------------------------------------------------------------------
// The desired tree: what the window currently shows, in the daemon's shape
// ---------------------------------------------------------------------------

/// One tab as the window wants the daemon to hold it.
#[derive(Debug, Clone)]
pub(crate) struct DesiredTab {
    pub id: TabId,
    pub name: Option<String>,
    pub group: Option<String>,
    pub root: DesiredNode,
}

/// A pane tree whose leaves carry the [`PaneSeed`] that introduces them, so an
/// op that first mentions a pane has its birth certificate in hand.
#[derive(Debug, Clone)]
pub(crate) enum DesiredNode {
    Leaf {
        pane: u64,
        seed: PaneSeed,
    },
    Split {
        axis: TreeAxis,
        ratio: f32,
        a: Box<DesiredNode>,
        b: Box<DesiredNode>,
    },
}

impl DesiredNode {
    /// The first (top/left-most) leaf — the anchor every split materializes
    /// around.
    fn first_leaf(&self) -> (&u64, &PaneSeed) {
        match self {
            DesiredNode::Leaf { pane, seed } => (pane, seed),
            DesiredNode::Split { a, .. } => a.first_leaf(),
        }
    }

    /// The plain tree shape, for comparing against a mirror tab's root.
    fn to_pane_node(&self) -> PaneNode {
        match self {
            DesiredNode::Leaf { pane, .. } => PaneNode::Leaf { pane: *pane },
            DesiredNode::Split { axis, ratio, a, b } => PaneNode::Split {
                axis: *axis,
                ratio: *ratio,
                a: Box::new(a.to_pane_node()),
                b: Box::new(b.to_pane_node()),
            },
        }
    }

    /// The seed of the leaf holding `pane`.
    fn seed_of(&self, pane: u64) -> Option<&PaneSeed> {
        match self {
            DesiredNode::Leaf { pane: p, seed } => (*p == pane).then_some(seed),
            DesiredNode::Split { a, b, .. } => a.seed_of(pane).or_else(|| b.seed_of(pane)),
        }
    }
}

/// Read the window's tabs into the daemon's shape. Tabs with nothing
/// representable yet (every pane still spawning) are omitted from the desired
/// list — but their identities are answered separately as *held*: the tab is
/// occupied, its panes just have no ids yet, and a diff that read its absence
/// as "closed" would delete the daemon tab (and spend the very records) a
/// revival in flight is about to replace.
pub(crate) fn desired_tabs(
    app: &Tty7App,
    cx: &App,
) -> (Vec<DesiredTab>, Option<TabId>, Vec<TabId>) {
    let remote = WorkspaceStore::all(cx)
        .get(app.workspace)
        .is_some_and(|w| w.is_remote());
    let mut out = Vec::new();
    let mut active = None;
    let mut held = Vec::new();
    for (index, tab) in app.tabs.iter().enumerate() {
        let Some(root) = desired_node(&tab.pane, remote, cx) else {
            held.push(tab.tree_id.get());
            continue;
        };
        let id = tab.tree_id.get();
        if index == app.active {
            active = Some(id);
        }
        out.push(DesiredTab {
            id,
            name: tab.name.clone(),
            group: tab
                .sidebar_group
                .borrow()
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned()),
            root,
        });
    }
    (out, active, held)
}

/// One GUI pane node, in tree shape. `None` for the unrepresentable: a fresh
/// spawn with no pane id yet, and — in a remote window — a native-SSH leaf,
/// whose pane lives in *this* client's daemon and so cannot be named in the
/// remote machine's tree (its id would collide with an unrelated pane there).
fn desired_node(pane: &Pane, remote_window: bool, cx: &App) -> Option<DesiredNode> {
    match pane {
        Pane::Leaf(PaneSlot::Ready(view)) => {
            let view = view.read(cx);
            let ssh_spec = view.ssh_spec();
            if remote_window && ssh_spec.is_some() {
                return None;
            }
            let agent = view.agent().map(|agent| {
                let session = view.agent_session();
                AgentFacts {
                    agent,
                    session_id: session.as_ref().and_then(|s| s.session_id.clone()),
                    launch_argv: session.as_ref().and_then(|s| s.launch_argv.clone()),
                    status: None,
                }
            });
            Some(DesiredNode::Leaf {
                pane: view.pane_id,
                seed: PaneSeed {
                    pane: view.pane_id,
                    cwd: view
                        .spawnable_cwd()
                        .map(|p| p.to_string_lossy().into_owned()),
                    ssh_spec,
                    agent,
                },
            })
        }
        Pane::Leaf(PaneSlot::Connecting(pending)) => {
            let spawn = &pending.read(cx).spawn;
            let pane = spawn.restore_pane?;
            let agent = spawn.agent.map(|agent| AgentFacts {
                agent,
                session_id: spawn.agent_session_id.clone(),
                launch_argv: spawn.agent_launch_argv.clone(),
                status: None,
            });
            Some(DesiredNode::Leaf {
                pane,
                seed: PaneSeed {
                    pane,
                    cwd: spawn
                        .working_directory
                        .as_ref()
                        .map(|p| p.to_string_lossy().into_owned()),
                    ssh_spec: None,
                    agent,
                },
            })
        }
        Pane::Split {
            axis, a, b, ratio, ..
        } => {
            let left = desired_node(a, remote_window, cx);
            let right = desired_node(b, remote_window, cx);
            match (left, right) {
                (Some(a), Some(b)) => Some(DesiredNode::Split {
                    axis: match axis {
                        gpui::Axis::Horizontal => TreeAxis::Horizontal,
                        gpui::Axis::Vertical => TreeAxis::Vertical,
                    },
                    ratio: ratio.get(),
                    a: Box::new(a),
                    b: Box::new(b),
                }),
                // One side has nothing to say yet: the other stands where the
                // split will be, exactly as the daemon would collapse it.
                (one, other) => one.or(other),
            }
        }
        Pane::Empty => None,
    }
}

// ---------------------------------------------------------------------------
// The mirror, and the diff that recovers operations from it
// ---------------------------------------------------------------------------

/// What the daemon's copy of this workspace looked like after the last
/// operation this window sent (or the last pull).
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct WsMirror {
    pub tabs: Vec<TreeTab>,
    pub active: Option<TabId>,
}

/// Diff the window's desired state against the mirror, answering the operation
/// sequence that turns one into the other — and advancing the mirror to the
/// predicted post-state as it goes.
///
/// `workspace` is the machine-side id the ops carry.
/// How much of the tree a window's diff may claim to speak for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum SyncScope {
    /// The window has seen the tree (it was hydrated from it, or the tree was
    /// empty when it primed): its state is the whole story, and tabs it does
    /// not show are tabs to close.
    Full,
    /// The window has **not** seen the tree — it opened empty ahead of a pull
    /// that has not landed (or was skipped). Its tabs are additions and edits,
    /// never evidence of absence: a diff that closed tree tabs such a window
    /// simply never displayed would eat another session's layout.
    Additive,
}

pub(crate) fn diff(
    workspace: WorkspaceId,
    mirror: &mut WsMirror,
    desired: &[DesiredTab],
    desired_active: Option<TabId>,
    scope: SyncScope,
    held: &[TabId],
) -> Vec<ControlRequest> {
    let mut ops = Vec::new();

    // Tabs that are gone. Position by position so the active-tab heal below
    // sees the same intermediate states the server will. Only a window that
    // has seen the tree may prune — see [`SyncScope`] — and a *held* tab (its
    // panes are mid-spawn, so it is invisible in `desired` without being
    // absent) is never pruned.
    if scope == SyncScope::Full {
        let mut index = 0;
        while index < mirror.tabs.len() {
            let id = mirror.tabs[index].id;
            if desired.iter().any(|t| t.id == id) || held.contains(&id) {
                index += 1;
                continue;
            }
            let closed = mirror.tabs.remove(index);
            ops.push(ControlRequest::TabClose {
                workspace,
                tab: closed.id,
            });
            heal_active(mirror, index);
        }
    }

    // New tabs and per-tab reconciliation, in the window's order. An additive
    // window appends its new tabs rather than claiming positions among tabs it
    // has never seen.
    for (index, want) in desired.iter().enumerate() {
        match mirror.tabs.iter().position(|t| t.id == want.id) {
            None => {
                let at = match scope {
                    SyncScope::Full => index,
                    SyncScope::Additive => mirror.tabs.len(),
                };
                create_tab(workspace, mirror, at, want, &mut ops);
            }
            Some(at) => reconcile_tab(workspace, mirror, at, want, &mut ops),
        }
    }

    // With any tab held, positions are ambiguous (a held tab occupies a slot
    // the desired list cannot see), so ordering and activation wait for the
    // save that follows the spawns landing.
    if scope == SyncScope::Additive || !held.is_empty() {
        return ops;
    }

    // Order: fix each position left to right. The tab moved is always to the
    // right of the slot it moves into, so earlier fixes stay fixed.
    for (index, want) in desired.iter().enumerate() {
        let at = mirror
            .tabs
            .iter()
            .position(|t| t.id == want.id)
            .expect("every desired tab exists after the passes above");
        if at != index {
            let tab = mirror.tabs.remove(at);
            mirror.tabs.insert(index, tab);
            ops.push(ControlRequest::TabMove {
                workspace,
                tab: want.id,
                to: index as u64,
            });
        }
    }

    // Which tab is active.
    if let Some(active) = desired_active
        && mirror.active != Some(active)
        && mirror.tabs.iter().any(|t| t.id == active)
    {
        mirror.active = Some(active);
        ops.push(ControlRequest::WorkspaceSetActiveTab {
            workspace,
            tab: active,
        });
    }

    ops
}

/// The server's active-tab heal, replayed on the mirror: after the tab at
/// `removed` left, a dangling active id re-points to the neighbour that slid
/// into its place (or the new last tab).
fn heal_active(mirror: &mut WsMirror, removed: usize) {
    let named = mirror
        .active
        .is_some_and(|active| mirror.tabs.iter().any(|t| t.id == active));
    if named {
        return;
    }
    if mirror.tabs.is_empty() {
        mirror.active = None;
        return;
    }
    mirror.active = Some(mirror.tabs[removed.min(mirror.tabs.len() - 1)].id);
}

/// Emit the ops that create `want` whole: `TabCreate` anchored on its first
/// leaf, then one `PaneSplit` per split, then the labels.
fn create_tab(
    workspace: WorkspaceId,
    mirror: &mut WsMirror,
    index: usize,
    want: &DesiredTab,
    ops: &mut Vec<ControlRequest>,
) {
    let (first, seed) = want.root.first_leaf();
    ops.push(ControlRequest::TabCreate {
        workspace,
        at: Some(index as u64),
        pane: seed.clone(),
        tab: Some(want.id),
    });
    let mut root = PaneNode::Leaf { pane: *first };
    materialize_splits(workspace, &want.root, &mut root, ops);
    if want.name.is_some() {
        ops.push(ControlRequest::TabRename {
            workspace,
            tab: want.id,
            name: want.name.clone(),
        });
    }
    if want.group.is_some() {
        ops.push(ControlRequest::TabSetGroup {
            workspace,
            tab: want.id,
            group: want.group.clone(),
        });
    }
    mirror.tabs.insert(
        index.min(mirror.tabs.len()),
        TreeTab {
            id: want.id,
            name: want.name.clone(),
            sidebar_group: want.group.clone(),
            root,
        },
    );
    // A created tab is active on the server; the final active pass corrects
    // this when the window says otherwise.
    mirror.active = Some(want.id);
}

/// Turn the single leaf standing where `want` goes into `want`'s whole split
/// structure, top split first — each split replaces the leaf that anchors its
/// left side, exactly as the server's `split_leaf` will.
fn materialize_splits(
    workspace: WorkspaceId,
    want: &DesiredNode,
    root: &mut PaneNode,
    ops: &mut Vec<ControlRequest>,
) {
    let DesiredNode::Split { axis, ratio, a, b } = want else {
        return;
    };
    let (anchor, _) = a.first_leaf();
    let (new, seed) = b.first_leaf();
    ops.push(ControlRequest::PaneSplit {
        workspace,
        pane: *anchor,
        axis: *axis,
        ratio: *ratio,
        new: seed.clone(),
        first: false,
    });
    root.split_leaf(*anchor, *new, *axis, *ratio, false);
    materialize_splits(workspace, a, root, ops);
    materialize_splits(workspace, b, root, ops);
}

/// Bring one existing tab in line: labels field by field, then the pane tree —
/// by the smallest op that explains the change, or by rebuilding the tab when
/// no single op does (a swap, a multi-pane rearrangement).
fn reconcile_tab(
    workspace: WorkspaceId,
    mirror: &mut WsMirror,
    at: usize,
    want: &DesiredTab,
    ops: &mut Vec<ControlRequest>,
) {
    {
        let tab = &mut mirror.tabs[at];
        if tab.name != want.name {
            tab.name = want.name.clone();
            ops.push(ControlRequest::TabRename {
                workspace,
                tab: want.id,
                name: want.name.clone(),
            });
        }
        if tab.sidebar_group != want.group {
            tab.sidebar_group = want.group.clone();
            ops.push(ControlRequest::TabSetGroup {
                workspace,
                tab: want.id,
                group: want.group.clone(),
            });
        }
    }

    let desired_root = want.root.to_pane_node();
    if mirror.tabs[at].root == desired_root {
        return;
    }
    if same_shape_and_panes(&mirror.tabs[at].root, &desired_root) {
        fix_ratios(
            workspace,
            want.id,
            &mut mirror.tabs[at].root,
            &desired_root,
            ops,
        );
        return;
    }

    let have = mirror.tabs[at].root.pane_ids();
    let wanted = desired_root.pane_ids();
    let added: Vec<u64> = wanted
        .iter()
        .copied()
        .filter(|p| !have.contains(p))
        .collect();
    let removed: Vec<u64> = have
        .iter()
        .copied()
        .filter(|p| !wanted.contains(p))
        .collect();

    let done = match (added.as_slice(), removed.as_slice()) {
        // One pane appeared: a split, if it reads as one.
        ([new], []) => try_single_split(workspace, mirror, at, want, &desired_root, *new, ops),
        // Panes left: close each, then check the shape agrees.
        ([], gone) if !gone.is_empty() => {
            for pane in gone {
                mirror.tabs[at].root.remove_leaf(*pane);
                ops.push(ControlRequest::PaneClose {
                    workspace,
                    pane: *pane,
                });
            }
            same_shape_and_panes(&mirror.tabs[at].root, &desired_root)
        }
        // One pane became another in place: the revival's rebind.
        ([new], [old]) => {
            let elsewhere = mirror
                .tabs
                .iter()
                .enumerate()
                .any(|(i, t)| i != at && t.root.contains(*new));
            let mut predicted = mirror.tabs[at].root.clone();
            predicted.replace_leaf(*old, *new);
            if !elsewhere && same_shape_and_panes(&predicted, &desired_root) {
                let seed = want
                    .root
                    .seed_of(*new)
                    .expect("the added pane is a desired leaf")
                    .clone();
                mirror.tabs[at].root = predicted;
                ops.push(ControlRequest::PaneReplace {
                    workspace,
                    old: *old,
                    new: seed,
                });
                true
            } else {
                false
            }
        }
        _ => false,
    };

    if done {
        fix_ratios(
            workspace,
            want.id,
            &mut mirror.tabs[at].root,
            &desired_root,
            ops,
        );
        return;
    }

    // Nothing smaller explains it (a swap, several panes moved at once):
    // rebuild the tab whole. The server broadcasts the same class of change as
    // one `TabClosed` + `TabCreated`+splits, which mirroring clients apply by
    // replacement — the granularity the delta contract already promises.
    let closed = mirror.tabs.remove(at);
    ops.push(ControlRequest::TabClose {
        workspace,
        tab: closed.id,
    });
    heal_active(mirror, at);
    create_tab(workspace, mirror, at, want, ops);
}

/// One added leaf, read as the split it was: find it in the desired tree,
/// check its sibling side is a leaf the mirror already holds, and check that
/// the tree minus the new leaf is the tree the mirror has. Emits the
/// `PaneSplit` and answers whether it took.
fn try_single_split(
    workspace: WorkspaceId,
    mirror: &mut WsMirror,
    at: usize,
    want: &DesiredTab,
    desired_root: &PaneNode,
    new: u64,
    ops: &mut Vec<ControlRequest>,
) -> bool {
    let Some((sibling, axis, ratio, first)) = split_site(desired_root, new) else {
        return false;
    };
    let mut predicted = mirror.tabs[at].root.clone();
    if !predicted.split_leaf(sibling, new, axis, ratio, first) {
        return false;
    }
    if !same_shape_and_panes(&predicted, desired_root) {
        return false;
    }
    let seed = want
        .root
        .seed_of(new)
        .expect("the added pane is a desired leaf")
        .clone();
    mirror.tabs[at].root = predicted;
    ops.push(ControlRequest::PaneSplit {
        workspace,
        pane: sibling,
        axis,
        ratio,
        new: seed,
        first,
    });
    true
}

/// Where `new` sits in `node`: the sibling **leaf** it split off from, with the
/// split's parameters. `None` when the sibling side is itself a split — the
/// server's `split_leaf` can only split a leaf, so that shape did not come from
/// one split and the caller falls back to a rebuild.
fn split_site(node: &PaneNode, new: u64) -> Option<(u64, TreeAxis, f32, bool)> {
    let PaneNode::Split { axis, ratio, a, b } = node else {
        return None;
    };
    match (&**a, &**b) {
        (PaneNode::Leaf { pane }, sibling) if *pane == new => {
            if let PaneNode::Leaf { pane: s } = sibling {
                return Some((*s, *axis, *ratio, true));
            }
            return None;
        }
        (sibling, PaneNode::Leaf { pane }) if *pane == new => {
            if let PaneNode::Leaf { pane: s } = sibling {
                return Some((*s, *axis, *ratio, false));
            }
            return None;
        }
        _ => {}
    }
    if a.contains(new) {
        split_site(a, new)
    } else if b.contains(new) {
        split_site(b, new)
    } else {
        None
    }
}

/// Same structure and the same pane at every position, ratios ignored.
fn same_shape_and_panes(a: &PaneNode, b: &PaneNode) -> bool {
    match (a, b) {
        (PaneNode::Leaf { pane: pa }, PaneNode::Leaf { pane: pb }) => pa == pb,
        (
            PaneNode::Split {
                axis: ax,
                a: aa,
                b: ab,
                ..
            },
            PaneNode::Split {
                axis: bx,
                a: ba,
                b: bb,
                ..
            },
        ) => ax == bx && same_shape_and_panes(aa, ba) && same_shape_and_panes(ab, bb),
        _ => false,
    }
}

/// Walk two same-shaped trees and emit a `PaneSetRatio` per split whose
/// divider moved, updating the mirror side in place.
fn fix_ratios(
    workspace: WorkspaceId,
    tab: TabId,
    mirror: &mut PaneNode,
    desired: &PaneNode,
    ops: &mut Vec<ControlRequest>,
) {
    fn walk(
        workspace: WorkspaceId,
        tab: TabId,
        mirror: &mut PaneNode,
        desired: &PaneNode,
        path: &mut Vec<Side>,
        ops: &mut Vec<ControlRequest>,
    ) {
        let (
            PaneNode::Split {
                ratio: mr,
                a: ma,
                b: mb,
                ..
            },
            PaneNode::Split {
                ratio: dr,
                a: da,
                b: db,
                ..
            },
        ) = (mirror, desired)
        else {
            return;
        };
        if (*mr - *dr).abs() > 1e-4 {
            *mr = *dr;
            ops.push(ControlRequest::PaneSetRatio {
                workspace,
                tab,
                path: path.clone(),
                ratio: *dr,
            });
        }
        path.push(Side::A);
        walk(workspace, tab, ma, da, path, ops);
        path.pop();
        path.push(Side::B);
        walk(workspace, tab, mb, db, path, ops);
        path.pop();
    }
    let mut path = Vec::new();
    walk(workspace, tab, mirror, desired, &mut path, ops);
}

// ---------------------------------------------------------------------------
// Per-window state, priming, and the op queue
// ---------------------------------------------------------------------------

/// Where one window's sync stands.
enum SyncPhase {
    /// No trustworthy mirror. `dirty` records that the window has state worth
    /// pushing once one arrives; `priming` that a pull is in flight.
    Unprimed {
        dirty: bool,
        priming: bool,
    },
    Primed(WsMirror),
}

struct WsState {
    sync: SyncPhase,
    /// Operations accepted but not yet sent. Drained strictly in order by one
    /// in-flight sender at a time — the ops are a serial narrative, and two
    /// senders would let a later op overtake the edit it builds on.
    queue: VecDeque<ControlRequest>,
    inflight: bool,
    /// Whether this window has *seen* the tree — hydrated from it, primed
    /// against an empty one, or deliberately declared authoritative (a
    /// restore-off open). Until then its diffs run [`SyncScope::Additive`]:
    /// a window that opened empty ahead of its pull must not read its own
    /// emptiness as "close everything".
    informed: bool,
}

impl Default for WsState {
    fn default() -> Self {
        WsState {
            sync: SyncPhase::Unprimed {
                dirty: false,
                priming: false,
            },
            queue: VecDeque::new(),
            inflight: false,
            informed: false,
        }
    }
}

/// Every window's sync state, by the *client's* workspace id.
#[derive(Default)]
pub(crate) struct TreeSync {
    windows: HashMap<WorkspaceId, WsState>,
}

impl Global for TreeSync {}

/// Push this window's current structure to its machine's tree. The single
/// entry point, called from `save_session` — i.e. from every structural change.
pub(crate) fn sync_window(app: &Tty7App, cx: &mut App) {
    let client_ws = app.workspace;
    // A window built outside the store (headless tests) has no machine to talk
    // to; skipping keeps those windows byte-for-byte what they were.
    if !cx.has_global::<crate::core::session::WorkspaceStore>() {
        return;
    }
    adopt_tab_ids(app, cx);
    let (desired, desired_active, held) = desired_tabs(app, cx);
    let machine_ws = tree_workspace_id(cx, client_ws);

    let state = cx
        .default_global::<TreeSync>()
        .windows
        .entry(client_ws)
        .or_default();
    match &mut state.sync {
        SyncPhase::Unprimed { dirty, priming } => {
            *dirty = true;
            if !*priming {
                *priming = true;
                start_prime(cx, client_ws);
            }
        }
        SyncPhase::Primed(mirror) => {
            let scope = if state.informed {
                SyncScope::Full
            } else {
                SyncScope::Additive
            };
            let ops = diff(machine_ws, mirror, &desired, desired_active, scope, &held);
            if !ops.is_empty() {
                let (tabs, active) = (mirror.tabs.clone(), mirror.active);
                state.queue.extend(ops);
                // Origin exclusion means this client never hears these ops
                // back, so the machine-wide mirror learns them here.
                let host = WorkspaceStore::host_of(cx, client_ws);
                crate::ui::machine_mirror::MachineMirrors::note_synced_workspace(
                    cx, host, machine_ws, tabs, active,
                );
                pump(cx, client_ws);
            }
        }
    }
}

/// Whether `client_ws`'s window has seen its machine's tree (or was declared
/// authoritative). The gate for destructive acts an *empty* window licenses —
/// a window whose hydration has not answered is empty because it is waiting,
/// not because the workspace is, and deleting the workspace on the strength of
/// that emptiness would take a populated tree with it.
pub(crate) fn window_is_informed(cx: &App, client_ws: WorkspaceId) -> bool {
    cx.try_global::<TreeSync>()
        .and_then(|t| t.windows.get(&client_ws))
        .is_some_and(|s| s.informed)
}

/// Declare that `client_ws`'s window speaks for the whole tree from here on —
/// the deliberate cases (a restore-off open, a window rebuilt from a source
/// the user chose) where the window's state *is* the intended layout.
pub(crate) fn mark_window_informed(cx: &mut App, client_ws: WorkspaceId) {
    cx.default_global::<TreeSync>()
        .windows
        .entry(client_ws)
        .or_default()
        .informed = true;
}

/// Give GUI tabs that don't yet know their tree identity the mirror's, matched
/// by the panes they hold. This is what keeps a window whose tabs were built
/// before the tree was pulled (any full rebuild) from closing and recreating
/// every daemon tab it already matches.
fn adopt_tab_ids(app: &Tty7App, cx: &App) {
    let Some(TreeSync { windows }) = cx.try_global::<TreeSync>() else {
        return;
    };
    let Some(WsState {
        sync: SyncPhase::Primed(mirror),
        ..
    }) = windows.get(&app.workspace)
    else {
        return;
    };
    let known: Vec<TabId> = app.tabs.iter().map(|t| t.tree_id.get()).collect();
    for tab in &app.tabs {
        let id = tab.tree_id.get();
        if mirror.tabs.iter().any(|m| m.id == id) {
            continue;
        }
        let panes: Vec<u64> = tab
            .pane
            .terminals()
            .iter()
            .map(|v| v.read(cx).pane_id)
            .collect();
        if panes.is_empty() {
            continue;
        }
        let Some(matched) = mirror
            .tabs
            .iter()
            .find(|m| !known.contains(&m.id) && panes.iter().any(|p| m.root.contains(*p)))
        else {
            continue;
        };
        tab.tree_id.set(matched.id);
    }
}

/// Drop a window's sync state — its window is closing or rebinding. The
/// machine's tree keeps the workspace; only this client's bookkeeping goes.
pub(crate) fn forget(cx: &mut App, client_ws: WorkspaceId) {
    if let Some(state) = cx.try_global::<TreeSync>() {
        let _ = state;
        cx.default_global::<TreeSync>().windows.remove(&client_ws);
    }
}

/// Fire one workspace-level operation (rename, touch, remove) at the machine
/// that owns `client_ws`'s tree. Fire-and-forget: these ops are idempotent
/// label writes with no ordering relationship to the structural queue.
pub(crate) fn fire_workspace_op(
    cx: &mut App,
    client_ws: WorkspaceId,
    op: impl FnOnce(WorkspaceId) -> ControlRequest,
) {
    if !cx.has_global::<crate::core::session::WorkspaceStore>() {
        return;
    }
    let host = WorkspaceStore::host_of(cx, client_ws);
    let machine_ws = tree_workspace_id(cx, client_ws);
    let request = op(machine_ws);
    // The op will not echo back to this client (origin exclusion), so the
    // machine-wide mirror folds it in here.
    crate::ui::machine_mirror::MachineMirrors::note_workspace_op(cx, host, &request);
    let Some(client) = control_for(cx, host) else {
        log::debug!("no control link to send {request:?}; the machine keeps its copy");
        return;
    };
    cx.background_executor()
        .spawn(async move {
            if let Err(e) = client.call(request.clone()) {
                log::warn!("workspace operation {request:?} was not accepted: {e}");
            }
        })
        .detach();
}

/// Set (or clear, with `None`) a workspace's user-chosen name. The name is
/// purely the machine's fact now — its tree is what every picker lists this
/// workspace from — so a rename is one fire-and-forget operation, and the
/// machine-wide mirror picks it up on the way out.
pub(crate) fn rename_workspace(cx: &mut App, client_ws: WorkspaceId, name: Option<String>) {
    fire_workspace_op(cx, client_ws, move |ws| ControlRequest::WorkspaceRename {
        workspace: ws,
        name,
    });
}

/// Pull the authoritative tree for this workspace (creating it on the machine
/// when it has none), then land it as the mirror.
fn start_prime(cx: &mut App, client_ws: WorkspaceId) {
    let host = WorkspaceStore::host_of(cx, client_ws);
    let machine_ws = tree_workspace_id(cx, client_ws);
    let Some(client) = control_for(cx, host) else {
        // Not reachable right now. Stay dirty; the next save retries, and a
        // reconnect-triggered save is what usually gets there first.
        if let Some(state) = cx.default_global::<TreeSync>().windows.get_mut(&client_ws)
            && let SyncPhase::Unprimed { priming, .. } = &mut state.sync
        {
            *priming = false;
        }
        return;
    };
    cx.spawn(async move |cx| {
        let outcome = cx
            .background_executor()
            .spawn(async move { pull_or_create(&client, machine_ws) })
            .await;
        cx.update(|cx| finish_prime(cx, client_ws, outcome));
    })
    .detach();
}

/// The blocking half of priming: the workspace's tree, or — when the machine
/// has never heard of it — the freshly created empty workspace. Created
/// nameless: the client keeps no name of its own any more, and the machine
/// derives a display name from the tabs the sync is about to send.
fn pull_or_create(client: &ControlClient, machine_ws: WorkspaceId) -> io::Result<WsMirror> {
    match client.call(ControlRequest::WorkspaceTree {
        workspace: machine_ws,
    }) {
        Ok(ReplyOk::WorkspaceTree(ws)) => Ok(WsMirror {
            tabs: ws.tabs,
            active: ws.active_tab,
        }),
        Ok(other) => Err(io::Error::other(format!(
            "WorkspaceTree answered {other:?}"
        ))),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            match client.call(ControlRequest::WorkspaceCreate {
                name: None,
                workspace: Some(machine_ws),
            })? {
                ReplyOk::WorkspaceTree(ws) => Ok(WsMirror {
                    tabs: ws.tabs,
                    active: ws.active_tab,
                }),
                other => Err(io::Error::other(format!(
                    "WorkspaceCreate answered {other:?}"
                ))),
            }
        }
        Err(e) => Err(e),
    }
}

fn finish_prime(cx: &mut App, client_ws: WorkspaceId, outcome: io::Result<WsMirror>) {
    // `get_mut`, never `entry`: a window forgotten while the pull was in
    // flight must not be resurrected as orphaned bookkeeping.
    let Some(state) = cx.default_global::<TreeSync>().windows.get_mut(&client_ws) else {
        return;
    };
    let was_dirty = matches!(state.sync, SyncPhase::Unprimed { dirty: true, .. });
    let landed = match outcome {
        Ok(mirror) => {
            // An empty tree has nothing an uninformed window could wrongly
            // prune, so priming against one is as good as having seen it.
            state.informed |= mirror.tabs.is_empty();
            let landed = (mirror.tabs.clone(), mirror.active);
            state.sync = SyncPhase::Primed(mirror);
            landed
        }
        Err(e) => {
            log::warn!("could not pull the tree for workspace {client_ws}: {e}");
            state.sync = SyncPhase::Unprimed {
                dirty: was_dirty,
                priming: false,
            };
            return;
        }
    };
    // The pull may have created the workspace on the machine, which this
    // client (the writer) hears no delta for.
    let host = WorkspaceStore::host_of(cx, client_ws);
    let machine_ws = tree_workspace_id(cx, client_ws);
    crate::ui::machine_mirror::MachineMirrors::note_synced_workspace(
        cx, host, machine_ws, landed.0, landed.1,
    );
    if !was_dirty {
        return;
    }
    // The window changed while the pull was in flight; diff it now.
    let Some(app) =
        crate::ui::windows::WindowRegistry::app_for(cx, client_ws).and_then(|app| app.upgrade())
    else {
        return;
    };
    app.update(cx, |app, cx| sync_window(app, cx));
}

/// Send everything queued, in order, one batch in flight at a time.
fn pump(cx: &mut App, client_ws: WorkspaceId) {
    let host = WorkspaceStore::host_of(cx, client_ws);
    let client = control_for(cx, host);
    let state = cx
        .default_global::<TreeSync>()
        .windows
        .entry(client_ws)
        .or_default();
    if state.inflight || state.queue.is_empty() {
        return;
    }
    let Some(client) = client else {
        desync(cx, client_ws, "the control link is down");
        return;
    };
    let batch: Vec<ControlRequest> = state.queue.drain(..).collect();
    state.inflight = true;
    cx.spawn(async move |cx| {
        let result = cx
            .background_executor()
            .spawn(async move {
                for op in batch {
                    if let Err(e) = client.call(op.clone()) {
                        return Err((op, e));
                    }
                }
                Ok(())
            })
            .await;
        cx.update(|cx| {
            if let Some(state) = cx.default_global::<TreeSync>().windows.get_mut(&client_ws) {
                state.inflight = false;
            }
            match result {
                // More may have queued behind this batch.
                Ok(()) => pump(cx, client_ws),
                Err((op, e)) => {
                    log::warn!("tree operation {op:?} failed: {e}; re-pulling the tree");
                    desync(cx, client_ws, "an operation was refused");
                }
            }
        });
    })
    .detach();
}

/// Prediction and reality disagreed (or the link went): drop what was queued,
/// forget the mirror, and re-pull. The next diff against the fresh pull
/// re-emits exactly the edits that still matter — one recovery path for every
/// failure mode.
fn desync(cx: &mut App, client_ws: WorkspaceId, why: &str) {
    log::info!("resynchronizing workspace {client_ws} with its machine ({why})");
    let Some(state) = cx.default_global::<TreeSync>().windows.get_mut(&client_ws) else {
        return;
    };
    state.queue.clear();
    state.inflight = false;
    state.sync = SyncPhase::Unprimed {
        dirty: true,
        priming: true,
    };
    start_prime(cx, client_ws);
}

// ---------------------------------------------------------------------------
// The read path: a window rebuilt from the machine's tree
// ---------------------------------------------------------------------------

/// One workspace of a pulled [`Machine`], lowered into the `Session` shape the
/// window builder already consumes — the tree's leaves joined with their pane
/// registry records.
///
/// The lowering *is* the revival decision, made per leaf by the daemon's own
/// liveness fact: a `live` pane keeps its id (the builder re-attaches), a dead
/// one lowers to an id-less leaf carrying the record's cwd, SSH spec and agent
/// resume — exactly the leaf shape that makes the builder spawn a successor.
/// The save that follows then diffs the successor's id against the mirror and
/// sends the `PaneReplace` that spends the old record.
pub(crate) fn session_from_tree(
    ws: &tty7_core::core::machine::Workspace,
    panes: &[PaneRecord],
) -> Session {
    let tabs: Vec<SessionTab> = ws
        .tabs
        .iter()
        .map(|tab| SessionTab {
            name: tab.name.clone(),
            tree_id: Some(tab.id),
            sidebar_group: tab.sidebar_group.clone().map(std::path::PathBuf::from),
            pane: session_pane_from_node(&tab.root, panes),
        })
        .collect();
    let active = ws
        .active_tab
        .and_then(|id| ws.tabs.iter().position(|t| t.id == id))
        .unwrap_or(0);
    Session { active, tabs }
}

fn session_pane_from_node(node: &PaneNode, panes: &[PaneRecord]) -> SessionPane {
    match node {
        PaneNode::Leaf { pane } => {
            let record = panes.iter().find(|p| p.id == *pane);
            let live = record.is_some_and(|r| r.live);
            let (cwd, ssh_spec, agent) = match record {
                Some(r) => (
                    r.cwd.clone().map(std::path::PathBuf::from),
                    r.ssh_spec.clone(),
                    r.agent.clone(),
                ),
                None => (None, None, None),
            };
            SessionPane::Leaf {
                cwd,
                // The daemon's liveness fact is the whole of the revival
                // decision: an id is only worth keeping if the daemon holds a
                // PTY for it *right now*.
                pane_id: live.then_some(*pane),
                ssh_spec,
                agent: agent.as_ref().map(|a| a.agent),
                agent_session_id: agent.as_ref().and_then(|a| a.session_id.clone()),
                agent_launch_argv: agent.as_ref().and_then(|a| a.launch_argv.clone()),
            }
        }
        PaneNode::Split { axis, ratio, a, b } => SessionPane::Split {
            axis: match axis {
                TreeAxis::Horizontal => crate::core::session::SessionAxis::Horizontal,
                TreeAxis::Vertical => crate::core::session::SessionAxis::Vertical,
            },
            ratio: *ratio,
            a: Box::new(session_pane_from_node(a, panes)),
            b: Box::new(session_pane_from_node(b, panes)),
        },
    }
}

/// How long an opening window waits for its machine's link before giving up on
/// the pull and staying empty. Generous against a slow daemon start; the local
/// link is normally up within one supervision tick.
const HYDRATE_LINK_DEADLINE: std::time::Duration = std::time::Duration::from_secs(15);
const HYDRATE_LINK_POLL: std::time::Duration = std::time::Duration::from_millis(200);

/// Fill an (empty) window from the machine's tree: pull `MachineGet`, prime
/// the mirror with the workspace's tabs, and rebuild the window from them —
/// re-attaching live panes, spawning successors for dead ones.
///
/// The window opens first and this runs behind it, because the pull is a round
/// trip that may have to wait out the link coming up; against the local daemon
/// it lands within milliseconds, so in practice the empty state is one frame.
///
/// A workspace the machine has never heard of is created, empty. There is no
/// fallback source any more: the client keeps no layout of its own, so what
/// the machine answers is the layout.
pub(crate) fn hydrate_window_from_tree(cx: &mut App, client_ws: WorkspaceId) {
    hydrate(cx, client_ws, Adopt::IfEmpty);
}

/// What a finished pull may do to the window.
#[derive(Clone, Copy, PartialEq)]
enum Adopt {
    /// Fill an empty window; a window with tabs wins over the pull (the user
    /// got there first). The open/restore path.
    IfEmpty,
    /// Replace the window's tabs with the pulled tree. The delta-fallback
    /// resync, where the window is known to have drifted.
    Replace,
}

fn hydrate(cx: &mut App, client_ws: WorkspaceId, adopt: Adopt) {
    let host = WorkspaceStore::host_of(cx, client_ws);
    let machine_ws = tree_workspace_id(cx, client_ws);
    {
        let state = cx
            .default_global::<TreeSync>()
            .windows
            .entry(client_ws)
            .or_default();
        state.sync = SyncPhase::Unprimed {
            dirty: false,
            priming: true,
        };
        // Same contract as `desync`: anything queued was computed against a
        // mirror this pull is about to replace, and letting it drain after
        // the snapshot would silently diverge the server from it.
        state.queue.clear();
    }
    cx.spawn(async move |cx| {
        // At launch the link is usually still dialing; wait it out briefly
        // rather than failing an open the supervisor will fix in a second.
        let deadline = std::time::Instant::now() + HYDRATE_LINK_DEADLINE;
        let client = loop {
            let client = cx.update(|cx| control_for(cx, host));
            match client {
                Some(client) => break Some(client),
                None if std::time::Instant::now() > deadline => break None,
                None => cx.background_executor().timer(HYDRATE_LINK_POLL).await,
            }
        };
        let Some(client) = client else {
            log::warn!("workspace {client_ws}: no link to its machine; opening empty");
            cx.update(|cx| {
                if let Some(state) = cx.default_global::<TreeSync>().windows.get_mut(&client_ws) {
                    if let SyncPhase::Unprimed { priming, .. } = &mut state.sync {
                        *priming = false;
                    }
                }
            });
            return;
        };
        let outcome = cx
            .background_executor()
            .spawn(async move { pull_workspace(&client, machine_ws) })
            .await;
        cx.update(|cx| finish_hydration(cx, client_ws, adopt, outcome));
    })
    .detach();
}

/// The blocking half: the whole machine (the tree plus the pane registry —
/// `WorkspaceTree` alone answers structure without the pane facts revival
/// needs), reduced to this workspace's mirror and session. A machine that has
/// no such workspace gets it created, empty. The machine rides along whole so
/// the caller can refresh the machine-wide mirror off a pull it already paid
/// for.
fn pull_workspace(
    client: &ControlClient,
    machine_ws: WorkspaceId,
) -> io::Result<(Machine, WsMirror, Session)> {
    let machine: Machine = match client.call(ControlRequest::MachineGet)? {
        ReplyOk::MachineTree(m) => *m,
        other => return Err(io::Error::other(format!("MachineGet answered {other:?}"))),
    };
    match machine.workspaces.iter().find(|w| w.id == machine_ws) {
        Some(ws) => {
            let mirror = WsMirror {
                tabs: ws.tabs.clone(),
                active: ws.active_tab,
            };
            let session = session_from_tree(ws, &machine.panes);
            Ok((machine, mirror, session))
        }
        None => {
            client.call(ControlRequest::WorkspaceCreate {
                name: None,
                workspace: Some(machine_ws),
            })?;
            Ok((machine, WsMirror::default(), Session::default()))
        }
    }
}

fn finish_hydration(
    cx: &mut App,
    client_ws: WorkspaceId,
    adopt: Adopt,
    outcome: io::Result<(Machine, WsMirror, Session)>,
) {
    let (machine, mirror, session) = match outcome {
        Ok(pulled) => pulled,
        Err(e) => {
            log::warn!("could not hydrate workspace {client_ws} from its machine: {e}");
            if let Some(state) = cx.default_global::<TreeSync>().windows.get_mut(&client_ws)
                && let SyncPhase::Unprimed { priming, .. } = &mut state.sync
            {
                *priming = false;
            }
            return;
        }
    };
    // The pull is a whole `MachineGet`; the machine-wide mirror gets it free.
    let host = WorkspaceStore::host_of(cx, client_ws);
    crate::ui::machine_mirror::MachineMirrors::install(cx, host, machine);
    let was_dirty = {
        // `get_mut`, never `entry` — same reason as `finish_prime`.
        let Some(state) = cx.default_global::<TreeSync>().windows.get_mut(&client_ws) else {
            return;
        };
        let dirty = matches!(state.sync, SyncPhase::Unprimed { dirty: true, .. });
        // An empty tree has nothing to adopt and nothing a window could
        // wrongly prune, so the window is as informed as it will ever be. A
        // non-empty tree informs the window only if the adopt below actually
        // runs — see the IfEmpty return.
        state.informed |= mirror.tabs.is_empty();
        state.sync = SyncPhase::Primed(mirror);
        dirty
    };
    let Some(app) =
        crate::ui::windows::WindowRegistry::app_for(cx, client_ws).and_then(|app| app.upgrade())
    else {
        return;
    };
    if adopt == Adopt::IfEmpty && !app.read(cx).tabs.is_empty() {
        // The user got there first (opened a tab into the empty window). Their
        // tabs win — but they have never seen the tree's, so the window stays
        // additive: its edits go up, tabs it never showed stay untouched.
        if was_dirty {
            app.update(cx, |app, cx| sync_window(app, cx));
        }
        return;
    }
    // An empty pull leaves an empty window empty — with the client's layout
    // cache retired there is nothing to import, and the machine answering
    // "no tabs" *is* the layout.
    if session.tabs.is_empty() && adopt == Adopt::IfEmpty {
        if was_dirty
            && let Some(app) =
                crate::ui::windows::WindowRegistry::app_for(cx, client_ws).and_then(|a| a.upgrade())
        {
            app.update(cx, |app, cx| sync_window(app, cx));
        }
        return;
    }
    let Some(handle) = crate::ui::windows::WindowRegistry::window_for(cx, client_ws) else {
        return;
    };
    log::info!(
        "rebuilding {} tab(s) of workspace {client_ws} from its machine's tree",
        session.tabs.len()
    );
    // The window is about to display the tree (or the import that stands in
    // for it); from here its diffs speak for the whole workspace.
    mark_window_informed(cx, client_ws);
    let _ = handle.update(cx, move |_, window, cx| {
        app.update(cx, |app, cx| {
            app.adopt_workspace(client_ws, session, window, cx)
        });
    });
}

// ---------------------------------------------------------------------------
// Incremental deltas: another writer edited a workspace this client shows
// ---------------------------------------------------------------------------

/// Land one [`LayoutDelta`] pushed by a machine: advance this client's mirror,
/// then the live window showing the workspace, if any.
///
/// The writer never hears its own operation back (origin exclusion), so every
/// delta arriving here is *another* client's edit — and because application
/// updates the window and the mirror in the same step, the next local diff
/// sees no difference and produces no echo.
///
/// Anything that will not apply cleanly — a tab the mirror does not know, a
/// window whose state has drifted — falls back to a full re-pull of the
/// workspace and a rebuild, the same recovery every other failure uses.
pub(crate) fn on_layout_delta(cx: &mut App, host: HostId, key: &str, delta: LayoutDelta) {
    // The machine-wide mirror hears every delta, windowed workspace or not —
    // it is what the picker and the menus read about workspaces no window
    // shows.
    crate::ui::machine_mirror::MachineMirrors::apply_delta(cx, host, key, &delta);
    // The event names the machine's workspace id; translate to the client's.
    let client_ws = if host.is_local() {
        key.parse::<WorkspaceId>().ok()
    } else {
        WorkspaceStore::all(cx)
            .views
            .iter()
            .find(|w| {
                w.host
                    .as_ref()
                    .is_some_and(|r| r.host_id() == host && r.workspace.to_string() == key)
            })
            .map(|w| w.id)
    };
    let Some(client_ws) = client_ws else {
        return;
    };

    // A preempted window is read-only *and must stay passive*: applying a
    // structural delta would attach to panes the usurping client just created
    // — and one pane has one subscriber, so that steals the active client's
    // streams as they work. The mirror goes stale instead, and taking the
    // workspace back re-pulls it whole.
    if crate::ui::remote_workspace::workspace_is_preempted(cx, client_ws) {
        if let Some(state) = cx.default_global::<TreeSync>().windows.get_mut(&client_ws) {
            state.sync = SyncPhase::Unprimed {
                dirty: false,
                priming: false,
            };
            state.queue.clear();
        }
        return;
    }

    let mirror_ok = match cx
        .default_global::<TreeSync>()
        .windows
        .get_mut(&client_ws)
        .map(|s| &mut s.sync)
    {
        Some(SyncPhase::Primed(mirror)) => apply_to_mirror(mirror, &delta),
        // No mirror yet: whatever pull is (or will be) in flight already
        // answers with a state that includes this delta.
        _ => true,
    };

    let Some(app) =
        crate::ui::windows::WindowRegistry::app_for(cx, client_ws).and_then(|a| a.upgrade())
    else {
        return;
    };
    let Some(handle) = crate::ui::windows::WindowRegistry::window_for(cx, client_ws) else {
        return;
    };
    let window_ok = handle
        .update(cx, |_, window, cx| {
            app.update(cx, |app, cx| app.apply_layout_delta(&delta, window, cx))
        })
        .unwrap_or(true);
    if !mirror_ok || !window_ok {
        log::info!(
            "workspace {client_ws}: delta {delta:?} did not apply cleanly; re-pulling the tree"
        );
        resync_window_from_tree(cx, client_ws);
    }
}

/// Advance the mirror by one delta. `false` means the delta names state the
/// mirror does not have — the caller re-pulls.
fn apply_to_mirror(mirror: &mut WsMirror, delta: &LayoutDelta) -> bool {
    match delta {
        // Workspace-level facts carry no tab structure.
        LayoutDelta::WorkspaceCreated { .. }
        | LayoutDelta::WorkspaceRenamed { .. }
        | LayoutDelta::WorkspaceTouched { .. }
        | LayoutDelta::WorkspaceDeleted
        | LayoutDelta::PaneFacts { .. } => true,
        LayoutDelta::ActiveTabChanged { tab } => {
            mirror.active = Some(*tab);
            true
        }
        LayoutDelta::TabCreated { at, tab } => {
            let at = (*at).min(mirror.tabs.len());
            mirror.tabs.insert(at, tab.clone());
            true
        }
        LayoutDelta::TabClosed { tab } => {
            let before = mirror.tabs.len();
            mirror.tabs.retain(|t| t.id != *tab);
            if mirror.tabs.is_empty() {
                mirror.active = None;
            }
            // The heal, when one happened, arrives as its own
            // ActiveTabChanged — the server promises that.
            mirror.tabs.len() != before
        }
        LayoutDelta::TabRenamed { tab, name } => {
            let Some(t) = mirror.tabs.iter_mut().find(|t| t.id == *tab) else {
                return false;
            };
            t.name = name.clone();
            true
        }
        LayoutDelta::TabRegrouped { tab, group } => {
            let Some(t) = mirror.tabs.iter_mut().find(|t| t.id == *tab) else {
                return false;
            };
            t.sidebar_group = group.clone();
            true
        }
        LayoutDelta::TabMoved { tab, to } => {
            let Some(from) = mirror.tabs.iter().position(|t| t.id == *tab) else {
                return false;
            };
            let moved = mirror.tabs.remove(from);
            mirror.tabs.insert((*to).min(mirror.tabs.len()), moved);
            true
        }
        LayoutDelta::TabRestructured { tab, .. } => {
            let Some(t) = mirror.tabs.iter_mut().find(|t| t.id == tab.id) else {
                return false;
            };
            *t = tab.clone();
            true
        }
        LayoutDelta::RatioChanged { tab, path, ratio } => {
            let Some(t) = mirror.tabs.iter_mut().find(|t| t.id == *tab) else {
                return false;
            };
            match t.root.descend_mut(path) {
                Some(PaneNode::Split { ratio: r, .. }) => {
                    *r = *ratio;
                    true
                }
                _ => false,
            }
        }
    }
}

/// Re-pull the workspace and rebuild its window from the result, replacing
/// whatever the window holds — the delta fallback.
pub(crate) fn resync_window_from_tree(cx: &mut App, client_ws: WorkspaceId) {
    hydrate(cx, client_ws, Adopt::Replace);
}

impl Tty7App {
    /// Apply one delta to this window. `false` when it cannot be applied
    /// cleanly, in which case the caller re-pulls and rebuilds.
    pub(crate) fn apply_layout_delta(
        &mut self,
        delta: &LayoutDelta,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) -> bool {
        let index_of = |tabs: &[crate::ui::app::Tab], id: TabId| {
            tabs.iter().position(|t| t.tree_id.get() == id)
        };
        let applied = match delta {
            // Another client naming the workspace needs nothing from the
            // window: the chip and the picker read the machine mirror, which
            // already applied the delta.
            LayoutDelta::WorkspaceCreated { .. }
            | LayoutDelta::WorkspaceTouched { .. }
            | LayoutDelta::WorkspaceRenamed { .. }
            | LayoutDelta::PaneFacts { .. } => true,
            // Deleting a workspace someone is looking at does not close their
            // window — a window is never closed by remote control. The next
            // structural edit here recreates the workspace on the machine.
            LayoutDelta::WorkspaceDeleted => {
                log::info!(
                    "workspace {} was deleted on its machine; keeping the window",
                    self.workspace
                );
                true
            }
            LayoutDelta::ActiveTabChanged { tab } => {
                if let Some(index) = index_of(&self.tabs, *tab) {
                    self.activate_from_delta(index, window, cx);
                }
                // A tab this window doesn't hold yet: its TabCreated may be a
                // spawn still in flight. Not worth a rebuild.
                true
            }
            LayoutDelta::TabCreated { at, tab } => {
                self.insert_tab_from_tree((*at).min(self.tabs.len()), tab, window, cx)
            }
            LayoutDelta::TabClosed { tab } => {
                if let Some(index) = index_of(&self.tabs, *tab) {
                    // The panes' views go; the panes themselves were the
                    // closing client's to kill. The active tab is tracked by
                    // identity, or closing a tab to its left would silently
                    // shift focus one tab over.
                    let active_id = self.tabs.get(self.active).map(|t| t.tree_id.get());
                    self.tabs.remove(index);
                    self.active = active_id
                        .and_then(|id| index_of(&self.tabs, id))
                        .unwrap_or_else(|| index.min(self.tabs.len().saturating_sub(1)));
                    self.maximized = None;
                    self.focus_active(window, cx);
                }
                true
            }
            LayoutDelta::TabRenamed { tab, name } => {
                if let Some(index) = index_of(&self.tabs, *tab) {
                    self.tabs[index].name = name.clone();
                }
                true
            }
            LayoutDelta::TabRegrouped { tab, group } => {
                if let Some(index) = index_of(&self.tabs, *tab) {
                    *self.tabs[index].sidebar_group.borrow_mut() =
                        group.clone().map(std::path::PathBuf::from);
                }
                true
            }
            LayoutDelta::TabMoved { tab, to } => {
                if let Some(from) = index_of(&self.tabs, *tab) {
                    let active_id = self.tabs.get(self.active).map(|t| t.tree_id.get());
                    let moved = self.tabs.remove(from);
                    self.tabs.insert((*to).min(self.tabs.len()), moved);
                    if let Some(id) = active_id
                        && let Some(index) = index_of(&self.tabs, id)
                    {
                        self.active = index;
                    }
                }
                true
            }
            LayoutDelta::TabRestructured { tab, .. } => {
                match index_of(&self.tabs, tab.id) {
                    Some(index) => self.rebuild_tab_from_tree(index, tab, window, cx),
                    // Restructure of a tab we never built — out of step.
                    None => false,
                }
            }
            LayoutDelta::RatioChanged { tab, path, ratio } => {
                if let Some(index) = index_of(&self.tabs, *tab) {
                    set_gui_ratio(&mut self.tabs[index].pane, path, *ratio)
                } else {
                    true
                }
            }
        };
        cx.notify();
        applied
    }

    /// Activate a tab because a delta said so — the parts of `activate` that
    /// move state, without the save that would echo the change back.
    fn activate_from_delta(
        &mut self,
        index: usize,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) {
        if self.active == index {
            return;
        }
        self.maximized = None;
        self.active = index;
        self.focus_active(window, cx);
    }

    /// Build one GUI tab from a tree tab whose panes are all live (they were
    /// just created by the writer), attaching each by id.
    fn insert_tab_from_tree(
        &mut self,
        at: usize,
        tab: &TreeTab,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) -> bool {
        let mut existing = HashMap::new();
        let Some(pane) = self.build_pane_from_tree(&tab.root, &mut existing, window, cx) else {
            return false;
        };
        let gui = crate::ui::app::Tab::from_tree(tab, pane);
        self.tabs.insert(at, gui);
        if self.active >= at && self.tabs.len() > 1 {
            self.active += 1;
        }
        true
    }

    /// Rebuild one tab's pane tree to match the machine's, **reusing** the
    /// views of panes the window already shows — re-attaching a pane this
    /// window holds would steal its own stream (one pane, one subscriber).
    fn rebuild_tab_from_tree(
        &mut self,
        index: usize,
        tab: &TreeTab,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) -> bool {
        let remote = WorkspaceStore::all(cx)
            .get(self.workspace)
            .is_some_and(|w| w.is_remote());
        let mut existing: HashMap<u64, PaneSlot> = HashMap::new();
        // Native-SSH leaves in a remote window hold panes in *this* client's
        // daemon: they are deliberately absent from the remote machine's tree
        // (their ids would collide with unrelated panes there), so the tree
        // this tab is rebuilt from cannot mention them. They are kept aside
        // and appended back as splits below — dropping their views would
        // orphan running local sessions the writer never touched.
        let mut ssh_slots: Vec<PaneSlot> = Vec::new();
        for slot in self.tabs[index].pane.leaves() {
            let id = match &slot {
                // Matching a native-SSH leaf's *local* id against remote ids
                // would rebind the SSH view onto an unrelated remote pane.
                PaneSlot::Ready(view) if remote && view.read(cx).ssh_spec().is_some() => {
                    ssh_slots.push(slot);
                    continue;
                }
                PaneSlot::Ready(view) => Some(view.read(cx).pane_id),
                PaneSlot::Connecting(pending) => pending.read(cx).spawn.restore_pane,
            };
            if let Some(id) = id {
                existing.insert(id, slot);
            }
        }
        let Some(pane) = self.build_pane_from_tree(&tab.root, &mut existing, window, cx) else {
            return false;
        };
        // The ssh leaves' places in the old split geometry are unknowable from
        // the delta (the tree never held them), so each comes back as a fresh
        // half-and-half split on the right — the shape a split created it in.
        let pane = ssh_slots.into_iter().fold(pane, |tree, slot| {
            Pane::split_node(gpui::Axis::Horizontal, 0.5, tree, Pane::Leaf(slot))
        });
        let gui = &mut self.tabs[index];
        gui.pane = pane;
        gui.name = tab.name.clone();
        *gui.sidebar_group.borrow_mut() = tab.sidebar_group.clone().map(std::path::PathBuf::from);
        self.maximized = None;
        // Slots left in `existing` belonged to panes the writer removed; their
        // views drop with the old tree, and killing the panes was the writer's
        // act, not ours.
        true
    }

    /// Lower a tree node into a GUI pane tree, taking views for known panes
    /// from `existing` and attaching to unknown (writer-created) ones by id.
    fn build_pane_from_tree(
        &self,
        node: &PaneNode,
        existing: &mut HashMap<u64, PaneSlot>,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) -> Option<Pane> {
        match node {
            PaneNode::Leaf { pane } => {
                if let Some(slot) = existing.remove(pane) {
                    return Some(Pane::Leaf(slot));
                }
                match crate::ui::app::new_terminal(
                    self.window_workspace(cx),
                    Some(self.workspace),
                    self.font_size,
                    None,
                    Some(*pane),
                    None,
                    window,
                    cx,
                ) {
                    Ok(slot) => Some(Pane::Leaf(slot)),
                    Err(e) => {
                        log::warn!("could not attach pane {pane} from a delta: {e}");
                        None
                    }
                }
            }
            PaneNode::Split { axis, ratio, a, b } => {
                let left = self.build_pane_from_tree(a, existing, window, cx);
                let right = self.build_pane_from_tree(b, existing, window, cx);
                match (left, right) {
                    (Some(a), Some(b)) => Some(Pane::split_node(
                        match axis {
                            TreeAxis::Horizontal => gpui::Axis::Horizontal,
                            TreeAxis::Vertical => gpui::Axis::Vertical,
                        },
                        *ratio,
                        a,
                        b,
                    )),
                    (one, other) => one.or(other),
                }
            }
        }
    }
}

/// Follow `path` through the GUI tree and move that split's divider.
fn set_gui_ratio(pane: &mut Pane, path: &[Side], ratio: f32) -> bool {
    match path.split_first() {
        None => match pane {
            Pane::Split { ratio: cell, .. } => {
                // The GUI renders ratios in a narrower band than the server
                // accepts; clamp like every GUI-side write does.
                cell.set(ratio.clamp(0.1, 0.9));
                true
            }
            _ => false,
        },
        Some((side, rest)) => match pane {
            Pane::Split { a, b, .. } => match side {
                Side::A => set_gui_ratio(a, rest, ratio),
                Side::B => set_gui_ratio(b, rest, ratio),
            },
            _ => false,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seed(pane: u64) -> PaneSeed {
        PaneSeed {
            pane,
            cwd: Some(format!("/work/{pane}")),
            ssh_spec: None,
            agent: None,
        }
    }

    fn leaf(pane: u64) -> DesiredNode {
        DesiredNode::Leaf {
            pane,
            seed: seed(pane),
        }
    }

    fn split(axis: TreeAxis, ratio: f32, a: DesiredNode, b: DesiredNode) -> DesiredNode {
        DesiredNode::Split {
            axis,
            ratio,
            a: Box::new(a),
            b: Box::new(b),
        }
    }

    fn tab(id: TabId, root: DesiredNode) -> DesiredTab {
        DesiredTab {
            id,
            name: None,
            group: None,
            root,
        }
    }

    /// Apply `ops`' effect is already folded into the mirror by `diff`; this
    /// asserts the mirror agrees with what the window wanted — the property
    /// the whole scheme rests on.
    fn assert_converged(mirror: &WsMirror, desired: &[DesiredTab]) {
        assert_eq!(mirror.tabs.len(), desired.len());
        for (m, d) in mirror.tabs.iter().zip(desired) {
            assert_eq!(m.id, d.id);
            assert_eq!(m.name, d.name);
            assert_eq!(m.sidebar_group, d.group);
            assert_eq!(m.root, d.root.to_pane_node());
        }
    }

    #[test]
    fn opening_the_first_tab_emits_a_create_carrying_the_client_identity() {
        let ws = WorkspaceId::new();
        let id = TabId::new();
        let mut mirror = WsMirror::default();
        let desired = vec![tab(id, leaf(7))];

        let ops = diff(ws, &mut mirror, &desired, Some(id), SyncScope::Full, &[]);
        assert_eq!(
            ops,
            vec![ControlRequest::TabCreate {
                workspace: ws,
                at: Some(0),
                pane: seed(7),
                tab: Some(id),
            }],
            "a created tab is active on the server, so no separate active op"
        );
        assert_converged(&mirror, &desired);
        assert_eq!(mirror.active, Some(id));
    }

    #[test]
    fn a_split_emits_one_pane_split_against_its_sibling() {
        let ws = WorkspaceId::new();
        let id = TabId::new();
        let mut mirror = WsMirror::default();
        let one = vec![tab(id, leaf(1))];
        diff(ws, &mut mirror, &one, Some(id), SyncScope::Full, &[]);

        let two = vec![tab(id, split(TreeAxis::Vertical, 0.5, leaf(1), leaf(2)))];
        let ops = diff(ws, &mut mirror, &two, Some(id), SyncScope::Full, &[]);
        assert_eq!(
            ops,
            vec![ControlRequest::PaneSplit {
                workspace: ws,
                pane: 1,
                axis: TreeAxis::Vertical,
                ratio: 0.5,
                new: seed(2),
                first: false,
            }]
        );
        assert_converged(&mirror, &two);
    }

    #[test]
    fn a_new_pane_on_the_upper_side_splits_with_first_set() {
        let ws = WorkspaceId::new();
        let id = TabId::new();
        let mut mirror = WsMirror::default();
        diff(
            ws,
            &mut mirror,
            &[tab(id, leaf(1))],
            Some(id),
            SyncScope::Full,
            &[],
        );

        let want = vec![tab(id, split(TreeAxis::Horizontal, 0.4, leaf(2), leaf(1)))];
        let ops = diff(ws, &mut mirror, &want, Some(id), SyncScope::Full, &[]);
        assert_eq!(
            ops,
            vec![ControlRequest::PaneSplit {
                workspace: ws,
                pane: 1,
                axis: TreeAxis::Horizontal,
                ratio: 0.4,
                new: seed(2),
                first: true,
            }]
        );
        assert_converged(&mirror, &want);
    }

    #[test]
    fn closing_a_pane_emits_pane_close_and_the_split_collapses() {
        let ws = WorkspaceId::new();
        let id = TabId::new();
        let mut mirror = WsMirror::default();
        diff(
            ws,
            &mut mirror,
            &[tab(id, split(TreeAxis::Vertical, 0.5, leaf(1), leaf(2)))],
            Some(id),
            SyncScope::Full,
            &[],
        );

        let want = vec![tab(id, leaf(1))];
        let ops = diff(ws, &mut mirror, &want, Some(id), SyncScope::Full, &[]);
        assert_eq!(
            ops,
            vec![ControlRequest::PaneClose {
                workspace: ws,
                pane: 2
            }]
        );
        assert_converged(&mirror, &want);
    }

    #[test]
    fn a_revived_leaf_emits_pane_replace_with_the_successors_seed() {
        let ws = WorkspaceId::new();
        let id = TabId::new();
        let mut mirror = WsMirror::default();
        diff(
            ws,
            &mut mirror,
            &[tab(id, split(TreeAxis::Vertical, 0.5, leaf(1), leaf(2)))],
            Some(id),
            SyncScope::Full,
            &[],
        );

        let want = vec![tab(id, split(TreeAxis::Vertical, 0.5, leaf(1), leaf(9)))];
        let ops = diff(ws, &mut mirror, &want, Some(id), SyncScope::Full, &[]);
        assert_eq!(
            ops,
            vec![ControlRequest::PaneReplace {
                workspace: ws,
                old: 2,
                new: seed(9),
            }]
        );
        assert_converged(&mirror, &want);
    }

    #[test]
    fn a_ratio_drag_emits_set_ratio_with_the_splits_path() {
        let ws = WorkspaceId::new();
        let id = TabId::new();
        let mut mirror = WsMirror::default();
        let nested = |r| {
            split(
                TreeAxis::Vertical,
                0.5,
                leaf(1),
                split(TreeAxis::Horizontal, r, leaf(2), leaf(3)),
            )
        };
        diff(
            ws,
            &mut mirror,
            &[tab(id, nested(0.5))],
            Some(id),
            SyncScope::Full,
            &[],
        );

        let want = vec![tab(id, nested(0.7))];
        let ops = diff(ws, &mut mirror, &want, Some(id), SyncScope::Full, &[]);
        assert_eq!(
            ops,
            vec![ControlRequest::PaneSetRatio {
                workspace: ws,
                tab: id,
                path: vec![Side::B],
                ratio: 0.7,
            }]
        );
        assert_converged(&mirror, &want);
    }

    #[test]
    fn closing_a_tab_emits_tab_close_and_heals_the_active_tab() {
        let ws = WorkspaceId::new();
        let (a, b) = (TabId::new(), TabId::new());
        let mut mirror = WsMirror::default();
        diff(
            ws,
            &mut mirror,
            &[tab(a, leaf(1)), tab(b, leaf(2))],
            Some(b),
            SyncScope::Full,
            &[],
        );

        let want = vec![tab(a, leaf(1))];
        let ops = diff(ws, &mut mirror, &want, None, SyncScope::Full, &[]);
        assert_eq!(
            ops,
            vec![ControlRequest::TabClose {
                workspace: ws,
                tab: b
            }],
            "the heal is the server's own rule, so no active op crosses"
        );
        assert_converged(&mirror, &want);
        assert_eq!(mirror.active, Some(a));
    }

    #[test]
    fn a_tab_reorder_emits_moves_that_land_the_windows_order() {
        let ws = WorkspaceId::new();
        let (a, b, c) = (TabId::new(), TabId::new(), TabId::new());
        let mut mirror = WsMirror::default();
        let before = [tab(a, leaf(1)), tab(b, leaf(2)), tab(c, leaf(3))];
        diff(ws, &mut mirror, &before, Some(c), SyncScope::Full, &[]);

        let want = vec![tab(c, leaf(3)), tab(a, leaf(1)), tab(b, leaf(2))];
        let ops = diff(ws, &mut mirror, &want, Some(c), SyncScope::Full, &[]);
        assert_eq!(
            ops,
            vec![ControlRequest::TabMove {
                workspace: ws,
                tab: c,
                to: 0
            }]
        );
        assert_converged(&mirror, &want);
    }

    #[test]
    fn renaming_and_regrouping_emit_their_label_ops() {
        let ws = WorkspaceId::new();
        let id = TabId::new();
        let mut mirror = WsMirror::default();
        diff(
            ws,
            &mut mirror,
            &[tab(id, leaf(1))],
            Some(id),
            SyncScope::Full,
            &[],
        );

        let mut named = tab(id, leaf(1));
        named.name = Some("build".into());
        named.group = Some("/repo".into());
        let want = vec![named];
        let ops = diff(ws, &mut mirror, &want, Some(id), SyncScope::Full, &[]);
        assert_eq!(
            ops,
            vec![
                ControlRequest::TabRename {
                    workspace: ws,
                    tab: id,
                    name: Some("build".into()),
                },
                ControlRequest::TabSetGroup {
                    workspace: ws,
                    tab: id,
                    group: Some("/repo".into()),
                },
            ]
        );
        assert_converged(&mirror, &want);
    }

    #[test]
    fn switching_tabs_emits_only_set_active_tab() {
        let ws = WorkspaceId::new();
        let (a, b) = (TabId::new(), TabId::new());
        let mut mirror = WsMirror::default();
        let both = [tab(a, leaf(1)), tab(b, leaf(2))];
        diff(ws, &mut mirror, &both, Some(b), SyncScope::Full, &[]);

        let ops = diff(ws, &mut mirror, &both, Some(a), SyncScope::Full, &[]);
        assert_eq!(
            ops,
            vec![ControlRequest::WorkspaceSetActiveTab {
                workspace: ws,
                tab: a
            }]
        );
        assert_eq!(mirror.active, Some(a));
    }

    #[test]
    fn a_swap_no_single_op_expresses_rebuilds_the_tab_whole() {
        let ws = WorkspaceId::new();
        let id = TabId::new();
        let mut mirror = WsMirror::default();
        diff(
            ws,
            &mut mirror,
            &[tab(id, split(TreeAxis::Vertical, 0.5, leaf(1), leaf(2)))],
            Some(id),
            SyncScope::Full,
            &[],
        );

        // The two panes trade places: same panes, same shape, different order.
        let want = vec![tab(id, split(TreeAxis::Vertical, 0.5, leaf(2), leaf(1)))];
        let ops = diff(ws, &mut mirror, &want, Some(id), SyncScope::Full, &[]);
        assert_eq!(
            ops,
            vec![
                ControlRequest::TabClose {
                    workspace: ws,
                    tab: id
                },
                ControlRequest::TabCreate {
                    workspace: ws,
                    at: Some(0),
                    pane: seed(2),
                    tab: Some(id),
                },
                ControlRequest::PaneSplit {
                    workspace: ws,
                    pane: 2,
                    axis: TreeAxis::Vertical,
                    ratio: 0.5,
                    new: seed(1),
                    first: false,
                },
            ]
        );
        assert_converged(&mirror, &want);
    }

    #[test]
    fn a_deep_tree_materializes_top_split_first_and_converges() {
        let ws = WorkspaceId::new();
        let id = TabId::new();
        let mut mirror = WsMirror::default();
        // ((1 | 2) over (3 | 4))
        let want = vec![tab(
            id,
            split(
                TreeAxis::Horizontal,
                0.6,
                split(TreeAxis::Vertical, 0.3, leaf(1), leaf(2)),
                split(TreeAxis::Vertical, 0.7, leaf(3), leaf(4)),
            ),
        )];
        let ops = diff(ws, &mut mirror, &want, Some(id), SyncScope::Full, &[]);
        assert_eq!(
            ops,
            vec![
                ControlRequest::TabCreate {
                    workspace: ws,
                    at: Some(0),
                    pane: seed(1),
                    tab: Some(id),
                },
                ControlRequest::PaneSplit {
                    workspace: ws,
                    pane: 1,
                    axis: TreeAxis::Horizontal,
                    ratio: 0.6,
                    new: seed(3),
                    first: false,
                },
                ControlRequest::PaneSplit {
                    workspace: ws,
                    pane: 1,
                    axis: TreeAxis::Vertical,
                    ratio: 0.3,
                    new: seed(2),
                    first: false,
                },
                ControlRequest::PaneSplit {
                    workspace: ws,
                    pane: 3,
                    axis: TreeAxis::Vertical,
                    ratio: 0.7,
                    new: seed(4),
                    first: false,
                },
            ]
        );
        assert_converged(&mirror, &want);
    }

    #[test]
    fn an_unchanged_window_emits_nothing() {
        let ws = WorkspaceId::new();
        let id = TabId::new();
        let mut mirror = WsMirror::default();
        let want = vec![tab(id, split(TreeAxis::Vertical, 0.5, leaf(1), leaf(2)))];
        diff(ws, &mut mirror, &want, Some(id), SyncScope::Full, &[]);
        assert_eq!(
            diff(ws, &mut mirror, &want, Some(id), SyncScope::Full, &[]),
            Vec::new()
        );
    }

    #[test]
    fn a_tab_whose_panes_are_all_still_spawning_is_held_not_closed() {
        let ws = WorkspaceId::new();
        let id = TabId::new();
        let mut mirror = WsMirror::default();
        diff(
            ws,
            &mut mirror,
            &[tab(id, leaf(1))],
            Some(id),
            SyncScope::Full,
            &[],
        );

        // The window's copy of the tab is mid-revival: every leaf is a spawn
        // with no pane id yet, so the tab is invisible in `desired` — but it
        // is *held*, not gone, and closing it would spend the very record the
        // landing spawn's PaneReplace needs.
        let ops = diff(ws, &mut mirror, &[], None, SyncScope::Full, &[id]);
        assert_eq!(ops, Vec::new());
        assert_eq!(mirror.tabs.len(), 1, "the daemon tab survives the wait");
    }

    #[test]
    fn an_additive_diff_never_closes_tabs_the_window_has_not_seen() {
        let ws = WorkspaceId::new();
        let (a, b) = (TabId::new(), TabId::new());
        let mut mirror = WsMirror::default();
        diff(
            ws,
            &mut mirror,
            &[tab(a, leaf(1)), tab(b, leaf(2))],
            Some(b),
            SyncScope::Full,
            &[],
        );

        // A window that opened empty ahead of its pull and grew one fresh tab:
        // its diff may add that tab, and must touch nothing else — reading its
        // ignorance as "close everything" would eat another session's layout.
        let fresh = TabId::new();
        let ops = diff(
            ws,
            &mut mirror,
            &[tab(fresh, leaf(9))],
            Some(fresh),
            SyncScope::Additive,
            &[],
        );
        assert_eq!(
            ops,
            vec![ControlRequest::TabCreate {
                workspace: ws,
                at: Some(2),
                pane: seed(9),
                tab: Some(fresh),
            }],
            "appended after the tabs it has not seen; nothing closed or moved"
        );
        assert_eq!(mirror.tabs.len(), 3);
    }

    #[test]
    fn deltas_advance_the_mirror_exactly_as_the_writers_operations_did() {
        // Writer A's mirror advances through `diff`; watcher B's advances by
        // applying the equivalent deltas. Both must land on the same tree —
        // that equality is what lets B mirror A without re-implementing A.
        let ws = WorkspaceId::new();
        let id = TabId::new();
        let mut watcher = WsMirror::default();

        let tree_tab = TreeTab {
            id,
            name: None,
            sidebar_group: None,
            root: PaneNode::Leaf { pane: 1 },
        };
        assert!(apply_to_mirror(
            &mut watcher,
            &LayoutDelta::TabCreated {
                at: 0,
                tab: tree_tab,
            },
        ));
        assert!(apply_to_mirror(
            &mut watcher,
            &LayoutDelta::ActiveTabChanged { tab: id },
        ));
        assert!(apply_to_mirror(
            &mut watcher,
            &LayoutDelta::TabRestructured {
                tab: TreeTab {
                    id,
                    name: None,
                    sidebar_group: None,
                    root: PaneNode::Split {
                        axis: TreeAxis::Vertical,
                        ratio: 0.5,
                        a: Box::new(PaneNode::Leaf { pane: 1 }),
                        b: Box::new(PaneNode::Leaf { pane: 2 }),
                    },
                },
                pane: None,
            },
        ));
        assert!(apply_to_mirror(
            &mut watcher,
            &LayoutDelta::RatioChanged {
                tab: id,
                path: Vec::new(),
                ratio: 0.7,
            },
        ));

        // The writer's own mirror, advanced by the diff for the same edits.
        let mut writer = WsMirror::default();
        diff(
            ws,
            &mut writer,
            &[tab(id, leaf(1))],
            Some(id),
            SyncScope::Full,
            &[],
        );
        let final_state = vec![tab(id, split(TreeAxis::Vertical, 0.7, leaf(1), leaf(2)))];
        diff(
            ws,
            &mut writer,
            &final_state,
            Some(id),
            SyncScope::Full,
            &[],
        );

        assert_eq!(watcher, writer);
    }

    #[test]
    fn a_delta_about_a_tab_the_mirror_does_not_hold_reports_itself() {
        let mut mirror = WsMirror::default();
        assert!(
            !apply_to_mirror(
                &mut mirror,
                &LayoutDelta::TabRenamed {
                    tab: TabId::new(),
                    name: Some("x".into()),
                },
            ),
            "an unappliable delta must say so, so the caller re-pulls"
        );
        assert!(!apply_to_mirror(
            &mut mirror,
            &LayoutDelta::TabClosed { tab: TabId::new() },
        ),);
    }

    #[test]
    fn a_live_leaf_keeps_its_pane_id_and_a_dead_one_lowers_to_a_revival_leaf() {
        use tty7_core::core::cli_agent::CLIAgent;
        let tab_id = TabId::new();
        let ws = tty7_core::core::machine::Workspace {
            tabs: vec![TreeTab {
                id: tab_id,
                name: Some("build".into()),
                sidebar_group: Some("/repo".into()),
                root: PaneNode::Split {
                    axis: TreeAxis::Vertical,
                    ratio: 0.3,
                    a: Box::new(PaneNode::Leaf { pane: 1 }),
                    b: Box::new(PaneNode::Leaf { pane: 2 }),
                },
            }],
            active_tab: Some(tab_id),
            ..Default::default()
        };
        let panes = vec![
            PaneRecord {
                id: 1,
                cwd: Some("/work".into()),
                live: true,
                ..PaneRecord::new(1)
            },
            PaneRecord {
                id: 2,
                cwd: Some("/work/api".into()),
                live: false,
                agent: Some(AgentFacts {
                    agent: CLIAgent::Claude,
                    session_id: Some("sid".into()),
                    launch_argv: Some(vec!["claude".into()]),
                    status: None,
                }),
                ..PaneRecord::new(2)
            },
        ];

        let session = session_from_tree(&ws, &panes);
        assert_eq!(session.tabs.len(), 1);
        assert_eq!(session.active, 0);
        let tab = &session.tabs[0];
        assert_eq!(
            tab.tree_id,
            Some(tab_id),
            "the daemon tab's identity rides along"
        );
        assert_eq!(tab.name.as_deref(), Some("build"));
        let SessionPane::Split { ratio, a, b, .. } = &tab.pane else {
            panic!("the split survives the lowering");
        };
        assert!((ratio - 0.3).abs() < 1e-6);
        match &**a {
            SessionPane::Leaf { pane_id, cwd, .. } => {
                assert_eq!(*pane_id, Some(1), "a live pane re-attaches by its id");
                assert_eq!(cwd.as_deref(), Some(std::path::Path::new("/work")));
            }
            _ => panic!("leaf"),
        }
        match &**b {
            SessionPane::Leaf {
                pane_id,
                cwd,
                agent,
                agent_session_id,
                ..
            } => {
                assert_eq!(
                    *pane_id, None,
                    "a dead pane's leaf takes the fresh-spawn path — that is the revival"
                );
                assert_eq!(cwd.as_deref(), Some(std::path::Path::new("/work/api")));
                assert_eq!(*agent, Some(CLIAgent::Claude));
                assert_eq!(agent_session_id.as_deref(), Some("sid"));
            }
            _ => panic!("leaf"),
        }
    }

    #[test]
    fn a_dangling_active_tab_in_the_pulled_tree_falls_back_to_the_first() {
        let ws = tty7_core::core::machine::Workspace {
            tabs: vec![TreeTab {
                id: TabId::new(),
                name: None,
                sidebar_group: None,
                root: PaneNode::Leaf { pane: 1 },
            }],
            active_tab: Some(TabId::new()),
            ..Default::default()
        };
        assert_eq!(session_from_tree(&ws, &[]).active, 0);
    }

    #[test]
    fn a_pane_id_reused_in_another_tab_is_never_read_as_a_replace() {
        let ws = WorkspaceId::new();
        let (a, b) = (TabId::new(), TabId::new());
        let mut mirror = WsMirror::default();
        diff(
            ws,
            &mut mirror,
            &[tab(a, leaf(1)), tab(b, leaf(2))],
            Some(b),
            SyncScope::Full,
            &[],
        );

        // Tab a now claims pane 2 (which tab b still holds) instead of pane 1
        // — a corrupt window state. `PaneReplace` would be refused by the
        // server (pane 2 is elsewhere in the tree), so the diff must not
        // choose it; the rebuild path handles it, and the server refusing
        // *that* too (duplicate pane) desyncs into a fresh pull.
        let want = vec![tab(a, leaf(2)), tab(b, leaf(2))];
        let ops = diff(ws, &mut mirror, &want, Some(b), SyncScope::Full, &[]);
        assert!(
            !ops.iter()
                .any(|op| matches!(op, ControlRequest::PaneReplace { .. })),
            "got {ops:?}"
        );
    }
}
