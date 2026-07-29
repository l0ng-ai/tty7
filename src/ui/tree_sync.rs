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
    AgentFacts, Axis as TreeAxis, Machine, PaneNode, PaneRecord, PaneSeed, Side, Tab as TreeTab,
    TabId,
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
/// [`RemoteConnections`](crate::ui::remote_connect::RemoteConnections), and
/// everything above this function stops caring which. `None` is always
/// transient (both holders have supervisors reconnecting), so callers treat it
/// as "not now": mark dirty and let the re-pull that follows reconnection
/// resend what still matters.
pub(crate) fn control_for(cx: &mut App, host: HostId) -> Option<Arc<ControlClient>> {
    if host.is_local() {
        crate::ui::local_link::LocalLink::client(cx)
    } else {
        crate::ui::remote_connect::RemoteConnections::get(cx, host)
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
/// representable yet (every pane still spawning) are omitted; they join the
/// tree on the save that follows their first pane landing.
pub(crate) fn desired_tabs(app: &Tty7App, cx: &App) -> (Vec<DesiredTab>, Option<TabId>) {
    let remote = WorkspaceStore::all(cx)
        .get(app.workspace)
        .is_some_and(|w| w.is_remote());
    let mut out = Vec::new();
    let mut active = None;
    for (index, tab) in app.tabs.iter().enumerate() {
        let Some(root) = desired_node(&tab.pane, remote, cx) else {
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
    (out, active)
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
pub(crate) fn diff(
    workspace: WorkspaceId,
    mirror: &mut WsMirror,
    desired: &[DesiredTab],
    desired_active: Option<TabId>,
) -> Vec<ControlRequest> {
    let mut ops = Vec::new();

    // Tabs that are gone. Position by position so the active-tab heal below
    // sees the same intermediate states the server will.
    let mut index = 0;
    while index < mirror.tabs.len() {
        if desired.iter().any(|t| t.id == mirror.tabs[index].id) {
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

    // New tabs and per-tab reconciliation, in the window's order.
    for (index, want) in desired.iter().enumerate() {
        match mirror.tabs.iter().position(|t| t.id == want.id) {
            None => create_tab(workspace, mirror, index, want, &mut ops),
            Some(at) => reconcile_tab(workspace, mirror, at, want, &mut ops),
        }
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
    let (desired, desired_active) = desired_tabs(app, cx);
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
            let ops = diff(machine_ws, mirror, &desired, desired_active);
            if !ops.is_empty() {
                state.queue.extend(ops);
                pump(cx, client_ws);
            }
        }
    }
}

/// Give GUI tabs that don't yet know their tree identity the mirror's, matched
/// by the panes they hold. This is what keeps a window whose tabs were built
/// before the tree was pulled (today's `session.json` restore; any full
/// rebuild) from closing and recreating every daemon tab it already matches.
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

/// Pull the authoritative tree for this workspace (creating it on the machine
/// when it has none), then land it as the mirror.
fn start_prime(cx: &mut App, client_ws: WorkspaceId) {
    let host = WorkspaceStore::host_of(cx, client_ws);
    let machine_ws = tree_workspace_id(cx, client_ws);
    let name = WorkspaceStore::all(cx)
        .get(client_ws)
        .and_then(|w| w.name.clone());
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
            .spawn(async move { pull_or_create(&client, machine_ws, name) })
            .await;
        cx.update(|cx| finish_prime(cx, client_ws, outcome));
    })
    .detach();
}

/// The blocking half of priming: the workspace's tree, or — when the machine
/// has never heard of it — the freshly created empty workspace.
fn pull_or_create(
    client: &ControlClient,
    machine_ws: WorkspaceId,
    name: Option<String>,
) -> io::Result<WsMirror> {
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
                name,
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
    let state = cx
        .default_global::<TreeSync>()
        .windows
        .entry(client_ws)
        .or_default();
    let was_dirty = matches!(state.sync, SyncPhase::Unprimed { dirty: true, .. });
    match outcome {
        Ok(mirror) => {
            state.sync = SyncPhase::Primed(mirror);
            if !was_dirty {
                return;
            }
            // The window changed while the pull was in flight; diff it now.
            let Some(app) = crate::ui::windows::WindowRegistry::app_for(cx, client_ws)
                .and_then(|app| app.upgrade())
            else {
                return;
            };
            app.update(cx, |app, cx| sync_window(app, cx));
        }
        Err(e) => {
            log::warn!("could not pull the tree for workspace {client_ws}: {e}");
            state.sync = SyncPhase::Unprimed {
                dirty: was_dirty,
                priming: false,
            };
        }
    }
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
    let state = cx
        .default_global::<TreeSync>()
        .windows
        .entry(client_ws)
        .or_default();
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
/// A workspace the machine has never heard of is created (empty) — and, as a
/// one-time courtesy to trees that predate the migration, an *empty* pull
/// falls back to the client's cached `session` copy: adopting it re-populates
/// the tree through the ordinary diff, which is the whole import.
pub(crate) fn hydrate_window_from_tree(cx: &mut App, client_ws: WorkspaceId) {
    let host = WorkspaceStore::host_of(cx, client_ws);
    let machine_ws = tree_workspace_id(cx, client_ws);
    let name = WorkspaceStore::all(cx)
        .get(client_ws)
        .and_then(|w| w.name.clone());
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
            .spawn(async move { pull_workspace(&client, machine_ws, name) })
            .await;
        cx.update(|cx| finish_hydration(cx, client_ws, outcome));
    })
    .detach();
}

/// The blocking half: the whole machine (the tree plus the pane registry —
/// `WorkspaceTree` alone answers structure without the pane facts revival
/// needs), reduced to this workspace's mirror and session. A machine that has
/// no such workspace gets it created, empty.
fn pull_workspace(
    client: &ControlClient,
    machine_ws: WorkspaceId,
    name: Option<String>,
) -> io::Result<(WsMirror, Session)> {
    let machine: Machine = match client.call(ControlRequest::MachineGet)? {
        ReplyOk::MachineTree(m) => *m,
        other => return Err(io::Error::other(format!("MachineGet answered {other:?}"))),
    };
    match machine.workspaces.iter().find(|w| w.id == machine_ws) {
        Some(ws) => Ok((
            WsMirror {
                tabs: ws.tabs.clone(),
                active: ws.active_tab,
            },
            session_from_tree(ws, &machine.panes),
        )),
        None => {
            client.call(ControlRequest::WorkspaceCreate {
                name,
                workspace: Some(machine_ws),
            })?;
            Ok((WsMirror::default(), Session::default()))
        }
    }
}

fn finish_hydration(
    cx: &mut App,
    client_ws: WorkspaceId,
    outcome: io::Result<(WsMirror, Session)>,
) {
    let (mirror, session) = match outcome {
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
    let was_dirty = {
        let state = cx
            .default_global::<TreeSync>()
            .windows
            .entry(client_ws)
            .or_default();
        let dirty = matches!(state.sync, SyncPhase::Unprimed { dirty: true, .. });
        state.sync = SyncPhase::Primed(mirror);
        dirty
    };
    let Some(app) =
        crate::ui::windows::WindowRegistry::app_for(cx, client_ws).and_then(|app| app.upgrade())
    else {
        return;
    };
    if !app.read(cx).tabs.is_empty() {
        // The user got there first (opened a tab into the empty window); their
        // window wins, and the sync below reconciles the tree to it.
        if was_dirty {
            app.update(cx, |app, cx| sync_window(app, cx));
        }
        return;
    }
    // The one-time import: a tree with nothing for this workspace, a client
    // with a cached layout — adopt the cache, and the adopt's own save
    // populates the tree through the ordinary diff.
    let session = if session.tabs.is_empty() {
        WorkspaceStore::all(cx)
            .get(client_ws)
            .map(|w| w.session.clone())
            .unwrap_or(session)
    } else {
        session
    };
    if session.tabs.is_empty() {
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
    let _ = handle.update(cx, move |_, window, cx| {
        app.update(cx, |app, cx| {
            app.adopt_workspace(client_ws, session, window, cx)
        });
    });
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

        let ops = diff(ws, &mut mirror, &desired, Some(id));
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
        diff(ws, &mut mirror, &one, Some(id));

        let two = vec![tab(id, split(TreeAxis::Vertical, 0.5, leaf(1), leaf(2)))];
        let ops = diff(ws, &mut mirror, &two, Some(id));
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
        diff(ws, &mut mirror, &[tab(id, leaf(1))], Some(id));

        let want = vec![tab(id, split(TreeAxis::Horizontal, 0.4, leaf(2), leaf(1)))];
        let ops = diff(ws, &mut mirror, &want, Some(id));
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
        );

        let want = vec![tab(id, leaf(1))];
        let ops = diff(ws, &mut mirror, &want, Some(id));
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
        );

        let want = vec![tab(id, split(TreeAxis::Vertical, 0.5, leaf(1), leaf(9)))];
        let ops = diff(ws, &mut mirror, &want, Some(id));
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
        diff(ws, &mut mirror, &[tab(id, nested(0.5))], Some(id));

        let want = vec![tab(id, nested(0.7))];
        let ops = diff(ws, &mut mirror, &want, Some(id));
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
        );

        let want = vec![tab(a, leaf(1))];
        let ops = diff(ws, &mut mirror, &want, None);
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
        diff(ws, &mut mirror, &before, Some(c));

        let want = vec![tab(c, leaf(3)), tab(a, leaf(1)), tab(b, leaf(2))];
        let ops = diff(ws, &mut mirror, &want, Some(c));
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
        diff(ws, &mut mirror, &[tab(id, leaf(1))], Some(id));

        let mut named = tab(id, leaf(1));
        named.name = Some("build".into());
        named.group = Some("/repo".into());
        let want = vec![named];
        let ops = diff(ws, &mut mirror, &want, Some(id));
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
        diff(ws, &mut mirror, &both, Some(b));

        let ops = diff(ws, &mut mirror, &both, Some(a));
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
        );

        // The two panes trade places: same panes, same shape, different order.
        let want = vec![tab(id, split(TreeAxis::Vertical, 0.5, leaf(2), leaf(1)))];
        let ops = diff(ws, &mut mirror, &want, Some(id));
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
        let ops = diff(ws, &mut mirror, &want, Some(id));
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
        diff(ws, &mut mirror, &want, Some(id));
        assert_eq!(diff(ws, &mut mirror, &want, Some(id)), Vec::new());
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
        );

        // Tab a now claims pane 2 (which tab b still holds) instead of pane 1
        // — a corrupt window state. `PaneReplace` would be refused by the
        // server (pane 2 is elsewhere in the tree), so the diff must not
        // choose it; the rebuild path handles it, and the server refusing
        // *that* too (duplicate pane) desyncs into a fresh pull.
        let want = vec![tab(a, leaf(2)), tab(b, leaf(2))];
        let ops = diff(ws, &mut mirror, &want, Some(b));
        assert!(
            !ops.iter()
                .any(|op| matches!(op, ControlRequest::PaneReplace { .. })),
            "got {ops:?}"
        );
    }
}
