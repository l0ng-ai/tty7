use std::collections::{HashMap, VecDeque};
use std::io;
use std::sync::Arc;

use gpui::{App, Global};
use gpui_component::WindowExt as _;
use tty7_core::core::machine::{
    AgentFacts, Axis as TreeAxis, LayoutDelta, Machine, PaneNode, PaneRecord, PaneSeed, Side,
    Tab as TreeTab, TabId,
};
use tty7_core::daemon::control::{ControlClient, ControlRequest, ReplyOk};
use tty7_core::host::HostId;

use crate::core::session::{Session, SessionPane, SessionTab, WorkspaceId, WorkspaceStore};
use crate::ui::app::Tty7App;
use crate::ui::i18n::{L10nKey, t};
use crate::ui::pane::{Pane, PaneSlot};

pub(crate) fn control_for(cx: &mut App, host: HostId) -> Option<Arc<ControlClient>> {
    if host.is_local() {
        crate::ui::local_link::LocalLink::client(cx)
    } else {
        crate::ui::remote_connect::HostLinks::get(cx, host)
            .map(|h| Arc::clone(h.client()))
            .filter(|c| c.is_connected())
    }
}

pub(crate) enum TreeLink {
    Ready(Arc<ControlClient>),
    Unserved,
    Down,
}

pub(crate) fn tree_control_for(cx: &mut App, host: HostId) -> TreeLink {
    classify_tree_link(control_for(cx, host))
}

fn classify_tree_link(client: Option<Arc<ControlClient>>) -> TreeLink {
    match client {
        Some(client)
            if client
                .hello()
                .has_feature(tty7_core::daemon::control::feature::MACHINE_TREE) =>
        {
            TreeLink::Ready(client)
        }
        Some(_) => TreeLink::Unserved,
        None => TreeLink::Down,
    }
}

fn tree_workspace_id(cx: &App, client_ws: WorkspaceId) -> WorkspaceId {
    WorkspaceStore::all(cx)
        .get(client_ws)
        .and_then(|w| w.host.as_ref())
        .map(|r| r.workspace)
        .unwrap_or(client_ws)
}

#[derive(Debug, Clone)]
pub(crate) struct DesiredTab {
    pub id: TabId,
    pub name: Option<String>,
    pub group: Option<String>,
    pub root: DesiredNode,
}

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
    fn first_leaf(&self) -> (&u64, &PaneSeed) {
        match self {
            DesiredNode::Leaf { pane, seed } => (pane, seed),
            DesiredNode::Split { a, .. } => a.first_leaf(),
        }
    }

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

    fn seed_of(&self, pane: u64) -> Option<&PaneSeed> {
        match self {
            DesiredNode::Leaf { pane: p, seed } => (*p == pane).then_some(seed),
            DesiredNode::Split { a, b, .. } => a.seed_of(pane).or_else(|| b.seed_of(pane)),
        }
    }
}

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
            if !(remote && every_leaf_is_native_ssh(&tab.pane, cx)) {
                held.push(tab.tree_id.get());
            }
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

fn every_leaf_is_native_ssh(pane: &Pane, cx: &App) -> bool {
    match pane {
        Pane::Leaf(PaneSlot::Ready(view)) => view.read(cx).ssh_spec().is_some(),
        Pane::Leaf(PaneSlot::Connecting(_)) | Pane::Empty => false,
        Pane::Split { a, b, .. } => {
            every_leaf_is_native_ssh(a, cx) && every_leaf_is_native_ssh(b, cx)
        }
    }
}

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
                (one, other) => one.or(other),
            }
        }
        Pane::Empty => None,
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct WsMirror {
    pub tabs: Vec<TreeTab>,
    pub active: Option<TabId>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum SyncScope {
    Full,
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

    if scope == SyncScope::Additive || !held.is_empty() {
        return ops;
    }

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
    mirror.active = Some(want.id);
}

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
        ([new], []) => try_single_split(workspace, mirror, at, want, &desired_root, *new, ops),
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

    let closed = mirror.tabs.remove(at);
    ops.push(ControlRequest::TabClose {
        workspace,
        tab: closed.id,
    });
    heal_active(mirror, at);
    create_tab(workspace, mirror, at, want, ops);
}

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

enum SyncPhase {
    Unprimed { dirty: bool, priming: bool },
    Primed(WsMirror),
}

struct WsState {
    sync: SyncPhase,
    queue: VecDeque<ControlRequest>,
    inflight: bool,
    informed: bool,
    epoch: u64,
    /// A hydration that failed and still owes this window its layout.
    ///
    /// The window is sitting empty because of it, so nothing may be pushed
    /// from it until the pull is retried — an empty window diffs into
    /// "close every tab" and would wipe the layout off the machine.
    rehydrate: Option<Adopt>,
    /// Whether this window has already been told why it opened empty.
    ///
    /// The retry is as quiet as the failure was, so a window whose machine
    /// never answers re-enters `hydrate` on every `sync_window` and would say
    /// the same thing again every fifteen seconds. Saying it once is the
    /// point; saying it on a loop is noise. Cleared once a pull lands, so a
    /// later outage is still worth a word.
    said_why_empty: bool,
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
            epoch: 0,
            rehydrate: None,
            said_why_empty: false,
        }
    }
}

#[derive(Default)]
pub(crate) struct TreeSync {
    windows: HashMap<WorkspaceId, WsState>,
}

impl Global for TreeSync {}

pub(crate) fn sync_window(app: &Tty7App, cx: &mut App) {
    let client_ws = app.workspace;
    if !cx.has_global::<crate::core::session::WorkspaceStore>() {
        return;
    }
    if crate::ui::remote_workspace::workspace_is_preempted(cx, client_ws) {
        return;
    }
    if let Some(adopt) = take_rehydrate(cx, client_ws, app.tabs.is_empty()) {
        hydrate(cx, client_ws, adopt);
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
                let host = WorkspaceStore::host_of(cx, client_ws);
                crate::ui::machine_mirror::MachineMirrors::note_synced_workspace(
                    cx, host, machine_ws, tabs, active,
                );
                pump(cx, client_ws);
            }
        }
    }
}

pub(crate) fn on_link_up(cx: &mut App, host: HostId) {
    for (workspace, app) in crate::ui::windows::WindowRegistry::open_windows(cx) {
        if WorkspaceStore::host_of(cx, workspace) != host {
            continue;
        }
        if let Some(app) = app.upgrade() {
            app.update(cx, |app, cx| sync_window(app, cx));
        }
    }
}

/// Claims a hydration owed to `client_ws`, if one is still outstanding.
///
/// A `Replace` retry is dropped once the window has tabs again: the user moved
/// on without us, and replaying the machine's older layout over their work
/// would be worse than never retrying at all.
fn take_rehydrate(cx: &mut App, client_ws: WorkspaceId, window_is_empty: bool) -> Option<Adopt> {
    let state = cx
        .default_global::<TreeSync>()
        .windows
        .get_mut(&client_ws)?;
    let adopt = state.rehydrate.take()?;
    (window_is_empty || adopt == Adopt::IfEmpty).then_some(adopt)
}

/// Whether a window with no tabs may delete `client_ws` outright — from the
/// machine's tree and from the store both.
///
/// Two independent things have to agree, because the window's own emptiness
/// cannot tell them apart: a workspace is empty when it genuinely holds
/// nothing, and equally when its layout failed to rebuild. Only the first is a
/// reason to delete anything, and the second has already cost a workspace with
/// ten live tabs in it.
///
/// So the window must be informed (it pulled a layout and put it up), *and* the
/// mirror — the machine's own account, which no local failure can empty — must
/// agree there is nothing there. An unprimed mirror knows nothing and answers
/// no: "I don't know" may never authorize a deletion.
pub(crate) fn workspace_is_disposable(cx: &App, client_ws: WorkspaceId) -> bool {
    let Some(state) = cx
        .try_global::<TreeSync>()
        .and_then(|t| t.windows.get(&client_ws))
    else {
        return false;
    };
    state.informed && matches!(&state.sync, SyncPhase::Primed(mirror) if mirror.tabs.is_empty())
}

pub(crate) fn mark_window_informed(cx: &mut App, client_ws: WorkspaceId) {
    cx.default_global::<TreeSync>()
        .windows
        .entry(client_ws)
        .or_default()
        .informed = true;
}

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

pub(crate) fn on_preempted(cx: &mut App, client_ws: WorkspaceId) {
    let Some(state) = cx.default_global::<TreeSync>().windows.get_mut(&client_ws) else {
        return;
    };
    state.sync = SyncPhase::Unprimed {
        dirty: false,
        priming: false,
    };
    state.queue.clear();
    state.informed = false;
    state.epoch += 1;
}

pub(crate) fn forget(cx: &mut App, client_ws: WorkspaceId) {
    if let Some(state) = cx.try_global::<TreeSync>() {
        let _ = state;
        cx.default_global::<TreeSync>().windows.remove(&client_ws);
    }
}

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
    crate::ui::machine_mirror::MachineMirrors::note_workspace_op(cx, host, &request);
    let client = match tree_control_for(cx, host) {
        TreeLink::Ready(client) => client,
        TreeLink::Unserved => {
            unsendable(
                &request,
                "this machine's server does not serve the workspace tree",
            );
            return;
        }
        TreeLink::Down => {
            unsendable(&request, "there is no control link to its machine");
            return;
        }
    };
    cx.background_executor()
        .spawn(async move {
            if let Err(e) = client.call(request.clone()) {
                unsendable(&request, &format!("the machine refused it: {e}"));
            }
        })
        .detach();
}

fn unsendable(request: &ControlRequest, why: &str) {
    match request {
        ControlRequest::WorkspaceRemove { workspace } => log::warn!(
            "workspace {workspace} was deleted here but not on its machine ({why}); \
             its entry stays in that machine's tree, where another client will still \
             see it — delete it again from a client that can reach the machine"
        ),
        other => log::debug!("{other:?} not sent ({why}); the next edit carries it"),
    }
}

pub(crate) fn rename_workspace(cx: &mut App, client_ws: WorkspaceId, name: Option<String>) {
    fire_workspace_op(cx, client_ws, move |ws| ControlRequest::WorkspaceRename {
        workspace: ws,
        name,
    });
}

fn start_prime(cx: &mut App, client_ws: WorkspaceId) {
    let host = WorkspaceStore::host_of(cx, client_ws);
    let machine_ws = tree_workspace_id(cx, client_ws);
    let client = match tree_control_for(cx, host) {
        TreeLink::Ready(client) => client,
        unavailable => {
            if matches!(unavailable, TreeLink::Unserved) {
                log::warn!(
                    "workspace {client_ws}: its machine's server does not serve the tree; \
                     the layout will not be synced"
                );
            }
            if let Some(state) = cx.default_global::<TreeSync>().windows.get_mut(&client_ws)
                && let SyncPhase::Unprimed { priming, .. } = &mut state.sync
            {
                *priming = false;
            }
            return;
        }
    };
    let epoch = cx
        .default_global::<TreeSync>()
        .windows
        .get(&client_ws)
        .map(|s| s.epoch)
        .unwrap_or(0);
    // Picked here, on the main thread, where the names already in use are
    // readable. It is only spent if the workspace turns out to be new.
    let fresh = fresh_workspace_name(cx, host);
    cx.spawn(async move |cx| {
        let outcome = cx
            .background_executor()
            .spawn(async move { pull_or_create(&client, machine_ws, fresh) })
            .await;
        cx.update(|cx| finish_prime(cx, client_ws, epoch, outcome));
    })
    .detach();
}

/// A codename no workspace on `host` is using. Beats leaving new workspaces
/// named after whatever directory their first shell happened to start in —
/// three of those in a switcher all read the same.
pub(crate) fn fresh_workspace_name(cx: &App, host: HostId) -> String {
    let mut taken: Vec<String> = Vec::new();
    if let Some(machine) = crate::ui::machine_mirror::MachineMirrors::machine(cx, host) {
        taken.extend(machine.workspaces.iter().filter_map(|w| w.name.clone()));
    }
    // Labels are the names the switcher actually shows, which for an unnamed
    // workspace is its directory. Counting those as taken is deliberately
    // generous — it only ever costs another roll of the dice.
    if cx.has_global::<WorkspaceStore>() {
        taken.extend(
            WorkspaceStore::all(cx)
                .views
                .iter()
                .filter(|w| w.host_id() == host)
                .filter_map(|w| w.label.clone()),
        );
    }
    tty7_core::core::codename::unique(|name| taken.iter().any(|t| t == name))
}

fn pull_or_create(
    client: &ControlClient,
    machine_ws: WorkspaceId,
    fresh: String,
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
                name: Some(fresh),
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

fn finish_prime(cx: &mut App, client_ws: WorkspaceId, epoch: u64, outcome: io::Result<WsMirror>) {
    let Some(state) = cx.default_global::<TreeSync>().windows.get_mut(&client_ws) else {
        return;
    };
    if state.epoch != epoch || !matches!(state.sync, SyncPhase::Unprimed { priming: true, .. }) {
        log::debug!("workspace {client_ws}: dropping a superseded tree pull");
        return;
    }
    let was_dirty = matches!(state.sync, SyncPhase::Unprimed { dirty: true, .. });
    let landed = match outcome {
        Ok(mirror) => {
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
    let host = WorkspaceStore::host_of(cx, client_ws);
    let machine_ws = tree_workspace_id(cx, client_ws);
    crate::ui::machine_mirror::MachineMirrors::note_synced_workspace(
        cx, host, machine_ws, landed.0, landed.1,
    );
    if !was_dirty {
        return;
    }
    let Some(app) =
        crate::ui::windows::WindowRegistry::app_for(cx, client_ws).and_then(|app| app.upgrade())
    else {
        return;
    };
    app.update(cx, |app, cx| sync_window(app, cx));
}

fn pump(cx: &mut App, client_ws: WorkspaceId) {
    let host = WorkspaceStore::host_of(cx, client_ws);
    let client = tree_control_for(cx, host);
    let state = cx
        .default_global::<TreeSync>()
        .windows
        .entry(client_ws)
        .or_default();
    if state.inflight || state.queue.is_empty() {
        return;
    }
    let client = match client {
        TreeLink::Ready(client) => client,
        TreeLink::Unserved => {
            desync(cx, client_ws, "the server does not serve the machine tree");
            return;
        }
        TreeLink::Down => {
            desync(cx, client_ws, "the control link is down");
            return;
        }
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
    state.epoch += 1;
    start_prime(cx, client_ws);
}

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
                // The id goes down whatever `live` says. That flag is a cached
                // fact about another process, written by whoever last observed
                // the pane and reloaded from disk as `false` on every server
                // start — so a quiet pane that nobody has observed since reads
                // as dead while its shell is very much alive. Believing it here
                // is what threw away live sessions on a workspace switch: the
                // id was erased, and the restore below had nothing to attach
                // to, so it spawned a fresh shell over a running one.
                //
                // Attaching is the thing that actually knows. `spawn_shell_
                // terminal_in` attaches when the pane is there and spawns fresh
                // when it is not, which is the same answer this filter was
                // trying to guess — except it is right. `live` stays a hint for
                // what to show, never the judge of what to destroy.
                pane_id: Some(*pane),
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

const HYDRATE_LINK_DEADLINE: std::time::Duration = std::time::Duration::from_secs(15);
const HYDRATE_LINK_POLL: std::time::Duration = std::time::Duration::from_millis(200);

pub(crate) fn hydrate_window_from_tree(cx: &mut App, client_ws: WorkspaceId) {
    hydrate(cx, client_ws, Adopt::IfEmpty);
}

#[derive(Clone, Copy, PartialEq)]
enum Adopt {
    IfEmpty,
    Replace,
}

fn hydrate(cx: &mut App, client_ws: WorkspaceId, adopt: Adopt) {
    let host = WorkspaceStore::host_of(cx, client_ws);
    let machine_ws = tree_workspace_id(cx, client_ws);
    let epoch = {
        let state = cx
            .default_global::<TreeSync>()
            .windows
            .entry(client_ws)
            .or_default();
        state.sync = SyncPhase::Unprimed {
            dirty: false,
            priming: true,
        };
        state.queue.clear();
        state.epoch += 1;
        // This attempt takes over the debt; it re-records it if it fails too.
        state.rehydrate = None;
        state.epoch
    };
    cx.spawn(async move |cx| {
        let deadline = std::time::Instant::now() + HYDRATE_LINK_DEADLINE;
        let client = loop {
            match cx.update(|cx| tree_control_for(cx, host)) {
                TreeLink::Ready(client) => break Some(client),
                TreeLink::Unserved => {
                    log::warn!(
                        "workspace {client_ws}: its machine's server does not serve the \
                         machine tree; opening empty"
                    );
                    break None;
                }
                TreeLink::Down if std::time::Instant::now() > deadline => {
                    log::warn!("workspace {client_ws}: no link to its machine; opening empty");
                    break None;
                }
                TreeLink::Down => cx.background_executor().timer(HYDRATE_LINK_POLL).await,
            }
        };
        let Some(client) = client else {
            cx.update(|cx| {
                // Only the attempt that still owns the window gets to speak: a
                // superseded one is being retried right now, and announcing an
                // emptiness someone else is already filling would be a lie by
                // the time it is read.
                //
                // A machine that answers late is normal for a remote one, and
                // the switcher already says so there. On this computer nothing
                // else would.
                if owe_rehydration(cx, client_ws, epoch, adopt)
                    && adopt == Adopt::IfEmpty
                    && host.is_local()
                {
                    say_why_the_window_is_empty(cx, client_ws);
                }
            });
            return;
        };
        let outcome = cx
            .background_executor()
            .spawn(async move { pull_workspace(&client, machine_ws) })
            .await;
        cx.update(|cx| finish_hydration(cx, client_ws, epoch, adopt, outcome));
    })
    .detach();
}

/// Tells the window it opened empty because its machine never answered.
///
/// An empty window is also what a window with no tabs looks like, and the retry
/// that would fill it in is as quiet as the failure was — so a server one
/// dialect behind reads as "tty7 lost my tabs" with nothing anywhere to say
/// otherwise. This is that "otherwise", said in the window it happened to.
fn say_why_the_window_is_empty(cx: &mut App, client_ws: WorkspaceId) {
    match cx.default_global::<TreeSync>().windows.get_mut(&client_ws) {
        Some(state) if !state.said_why_empty => state.said_why_empty = true,
        _ => return,
    }
    let Some(handle) = crate::ui::windows::WindowRegistry::window_for(cx, client_ws) else {
        return;
    };
    let _ = handle.update(cx, |_, window, cx| {
        window.push_notification(t(L10nKey::TreeWindowOpenedEmpty), cx);
    });
}

/// Records that a hydration failed and still owes `client_ws` its layout.
///
/// Nothing else recovers on its own: the window stays empty, and without this
/// the next `sync_window` would push that emptiness to the machine as "close
/// every tab". Instead the pull is retried the next time the window syncs —
/// which is what a reconnect does through `on_link_up`.
///
/// Returns whether the debt was taken on. A superseded attempt gets `false`:
/// a newer hydration owns the window now, and this one speaks for nothing.
fn owe_rehydration(cx: &mut App, client_ws: WorkspaceId, epoch: u64, adopt: Adopt) -> bool {
    let Some(state) = cx.default_global::<TreeSync>().windows.get_mut(&client_ws) else {
        return false;
    };
    if state.epoch != epoch {
        return false;
    }
    if let SyncPhase::Unprimed { priming, .. } = &mut state.sync {
        *priming = false;
    }
    state.rehydrate = Some(adopt);
    log::info!("workspace {client_ws}: will pull its layout again once its machine answers");
    true
}

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
            // The whole tree is already in hand, so the taken names can be read
            // straight off it rather than passed down from the main thread.
            let taken: Vec<&str> = machine
                .workspaces
                .iter()
                .filter_map(|w| w.name.as_deref())
                .collect();
            let name = tty7_core::core::codename::unique(|n| taken.contains(&n));
            client.call(ControlRequest::WorkspaceCreate {
                name: Some(name),
                workspace: Some(machine_ws),
            })?;
            Ok((machine, WsMirror::default(), Session::default()))
        }
    }
}

fn finish_hydration(
    cx: &mut App,
    client_ws: WorkspaceId,
    epoch: u64,
    adopt: Adopt,
    outcome: io::Result<(Machine, WsMirror, Session)>,
) {
    let current = cx
        .default_global::<TreeSync>()
        .windows
        .get(&client_ws)
        .map(|s| s.epoch);
    if current != Some(epoch) {
        log::debug!("workspace {client_ws}: dropping a superseded hydration");
        return;
    }
    let (machine, mirror, session) = match outcome {
        Ok(pulled) => pulled,
        Err(e) => {
            log::warn!("could not hydrate workspace {client_ws} from its machine: {e}");
            let _ = owe_rehydration(cx, client_ws, epoch, adopt);
            return;
        }
    };
    let host = WorkspaceStore::host_of(cx, client_ws);
    crate::ui::machine_mirror::MachineMirrors::install(cx, host, machine);
    let machine_was_empty = mirror.tabs.is_empty();
    let was_dirty = {
        let Some(state) = cx.default_global::<TreeSync>().windows.get_mut(&client_ws) else {
            return;
        };
        let dirty = matches!(state.sync, SyncPhase::Unprimed { dirty: true, .. });
        state.informed |= machine_was_empty;
        state.sync = SyncPhase::Primed(mirror);
        // The machine answered, so the explanation has been overtaken by events
        // and a later outage deserves its own.
        state.said_why_empty = false;
        dirty
    };
    let Some(app) =
        crate::ui::windows::WindowRegistry::app_for(cx, client_ws).and_then(|app| app.upgrade())
    else {
        return;
    };
    if adopt == Adopt::IfEmpty && !app.read(cx).tabs.is_empty() {
        // A full window over an empty tree has to write itself back, whether
        // or not an edit was waiting: the machine is missing tabs this window
        // is showing, and nothing else would ever put them there.
        //
        // Deliberately not limited to this machine. An empty tree means one of
        // two things and the answer is the same either way: locally the
        // workspace was removed under the window (`ws rm`, or another client),
        // and remotely the far end lost its records — a re-imaged box, a store
        // that was wiped. Writing the window back is what a reattach is for.
        // The panes it names may well be dead; the window already draws them
        // that way, and a tab the user can close beats a tab that silently
        // stops existing.
        if was_dirty || machine_was_empty {
            app.update(cx, |app, cx| sync_window(app, cx));
        }
        return;
    }
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
    let wanted = session.tabs.len();
    log::info!("rebuilding {wanted} tab(s) of workspace {client_ws} from its machine's tree");
    let _ = handle.update(cx, move |_, window, cx| {
        app.update(cx, |app, cx| {
            app.adopt_workspace(client_ws, session, window, cx)
        });
    });

    // Informed *after* the rebuild, and only if the rebuild produced something.
    //
    // The licence means "this window knows what belongs in this workspace", and
    // `switch_workspace` / `detach_workspace` read it as permission to delete a
    // workspace that has no tabs — from the machine tree and from the store
    // both. Granting it before the rebuild handed that permission to a window
    // whose rebuild had not happened yet, and a rebuild can produce nothing:
    // `tabs_from_session` drops any tab whose panes all fail to start, which is
    // what every tab does when the pane socket is unreachable. The window then
    // sat there, empty and authoritative, and the next switch deleted a
    // workspace with ten live tabs in it.
    //
    // Emptiness that came from a failure has to stay indistinguishable from not
    // knowing, because that is what it is.
    let rebuilt = crate::ui::windows::WindowRegistry::app_for(cx, client_ws)
        .and_then(|app| app.upgrade())
        .is_some_and(|app| !app.read(cx).tabs.is_empty());
    if rebuilt || wanted == 0 {
        mark_window_informed(cx, client_ws);
    } else {
        log::warn!(
            "workspace {client_ws}: none of its {wanted} tab(s) could be rebuilt; leaving the \
             window uninformed so the layout is not mistaken for an empty workspace"
        );
    }
}

/// Someone else removed this workspace from its machine — `tty7 ws rm`, or
/// another client.
///
/// With no window on it, it stops existing here too. Left in the store it
/// would keep its row in the switcher and open onto nothing, which is how a
/// workspace deleted from the CLI used to haunt the panel until a restart.
///
/// With a window on it, the window stays: `ws rm` leaves every pane running,
/// and closing the window would strand them with no way back. Pulling the
/// layout again is what makes that honest — finding the workspace gone is
/// exactly the case `pull_workspace` puts back under the same id, and the
/// window writes its tabs to it on the way out of the hydration.
fn on_workspace_deleted(cx: &mut App, client_ws: WorkspaceId) {
    if crate::ui::windows::WindowRegistry::window_for(cx, client_ws).is_none() {
        log::info!("workspace {client_ws} was deleted on its machine; forgetting it here too");
        forget(cx, client_ws);
        crate::core::session::WorkspaceStore::remove(cx, client_ws);
        crate::ui::windows::refresh_menu(cx);
        cx.refresh_windows();
        return;
    }
    log::info!(
        "workspace {client_ws} was deleted on its machine while a window still had it open; \
         putting it back under the same id"
    );
    hydrate(cx, client_ws, Adopt::IfEmpty);
}

pub(crate) fn on_layout_delta(cx: &mut App, host: HostId, key: &str, delta: LayoutDelta) {
    crate::ui::machine_mirror::MachineMirrors::apply_delta(cx, host, key, &delta);
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

    if crate::ui::remote_workspace::workspace_is_preempted(cx, client_ws) {
        on_preempted(cx, client_ws);
        return;
    }

    if matches!(delta, LayoutDelta::WorkspaceDeleted) {
        on_workspace_deleted(cx, client_ws);
        return;
    }

    let mirror_ok = match cx
        .default_global::<TreeSync>()
        .windows
        .get_mut(&client_ws)
        .map(|s| &mut s.sync)
    {
        Some(SyncPhase::Primed(mirror)) => apply_to_mirror(mirror, &delta),
        _ => return,
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
        return;
    }
    app.update(cx, |app, cx| sync_window(app, cx));
}

fn apply_to_mirror(mirror: &mut WsMirror, delta: &LayoutDelta) -> bool {
    match delta {
        // Nothing here is about a workspace's tab list, so the mirror is
        // already right. `WorkspaceDeleted` never reaches this far —
        // `on_layout_delta` hands it to `on_workspace_deleted` and returns —
        // and is listed only so a new delta cannot join this arm by accident.
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
            mirror.tabs.retain(|t| t.id != tab.id);
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

pub(crate) fn resync_window_from_tree(cx: &mut App, client_ws: WorkspaceId) {
    hydrate(cx, client_ws, Adopt::Replace);
}

impl Tty7App {
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
            LayoutDelta::WorkspaceCreated { .. }
            | LayoutDelta::WorkspaceTouched { .. }
            | LayoutDelta::WorkspaceRenamed { .. }
            | LayoutDelta::PaneFacts { .. } => true,
            // Unreachable: `on_layout_delta` hands a deletion to
            // `on_workspace_deleted` and returns before any window is asked. A
            // deletion is about whether this workspace still exists here at
            // all, which is not a question one window's tab list can answer.
            LayoutDelta::WorkspaceDeleted => true,
            LayoutDelta::ActiveTabChanged { tab } => {
                if let Some(index) = index_of(&self.tabs, *tab) {
                    self.activate_from_delta(index, window, cx);
                }
                true
            }
            LayoutDelta::TabCreated { at, tab } => {
                self.insert_tab_from_tree((*at).min(self.tabs.len()), tab, window, cx)
            }
            LayoutDelta::TabClosed { tab } => {
                if let Some(index) = index_of(&self.tabs, *tab) {
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
            LayoutDelta::TabRestructured { tab, .. } => match index_of(&self.tabs, tab.id) {
                Some(index) => self.rebuild_tab_from_tree(index, tab, window, cx),
                None => false,
            },
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

    fn insert_tab_from_tree(
        &mut self,
        at: usize,
        tab: &TreeTab,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) -> bool {
        if self.tabs.iter().any(|t| t.tree_id.get() == tab.id) {
            return true;
        }
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
        let mut ssh_slots: Vec<PaneSlot> = Vec::new();
        for slot in self.tabs[index].pane.leaves() {
            let id = match &slot {
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
        let pane = ssh_slots.into_iter().fold(pane, |tree, slot| {
            Pane::split_node(gpui::Axis::Horizontal, 0.5, tree, Pane::Leaf(slot))
        });
        let gui = &mut self.tabs[index];
        gui.pane = pane;
        gui.name = tab.name.clone();
        *gui.sidebar_group.borrow_mut() = tab.sidebar_group.clone().map(std::path::PathBuf::from);
        self.maximized = None;
        true
    }

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

fn set_gui_ratio(pane: &mut Pane, path: &[Side], ratio: f32) -> bool {
    match path.split_first() {
        None => match pane {
            Pane::Split { ratio: cell, .. } => {
                cell.set(ratio.clamp(0.05, 0.95));
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

    #[cfg(unix)]
    #[test]
    fn a_peer_without_the_machine_tree_bit_classifies_as_unserved() {
        use tty7_core::daemon::control::ControlHello;
        use tty7_core::host::local::LocalHost;
        use tty7_core::host::server::{Services, serve_with};

        let connect = |services: Services| {
            let (server, client) = std::os::unix::net::UnixStream::pair().unwrap();
            std::thread::spawn(move || {
                let _ = serve_with(server, LocalHost::new(), services);
            });
            let hello = ControlHello::host_rpc("test-token", "test-host");
            Arc::new(
                tty7_core::daemon::control::ControlClient::over_unix(
                    client,
                    &hello,
                    Box::new(|_| {}),
                )
                .unwrap(),
            )
        };

        let treeless = connect(Services::none());
        assert!(matches!(
            classify_tree_link(Some(treeless)),
            TreeLink::Unserved
        ));

        let dir = std::env::temp_dir().join(format!("tty7-treelink-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let store = tty7_core::core::machine::MachineStore::open(
            dir.join(tty7_core::core::machine::MACHINE_FILE),
        );
        let serving = connect(Services::with_machine(store));
        assert!(matches!(
            classify_tree_link(Some(serving)),
            TreeLink::Ready(_)
        ));

        assert!(matches!(classify_tree_link(None), TreeLink::Down));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[gpui::test]
    fn preemption_drops_the_mirror_the_queue_and_the_informed_licence(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(|cx| {
            let ws = WorkspaceId::new();
            {
                let state = cx
                    .default_global::<TreeSync>()
                    .windows
                    .entry(ws)
                    .or_default();
                state.sync = SyncPhase::Primed(WsMirror::default());
                state.informed = true;
                state.queue.push_back(ControlRequest::Ping);
            }
            on_preempted(cx, ws);
            let state = &cx.default_global::<TreeSync>().windows[&ws];
            assert!(matches!(
                state.sync,
                SyncPhase::Unprimed {
                    dirty: false,
                    priming: false,
                }
            ));
            assert!(
                state.queue.is_empty(),
                "queued ops belong to the lost session"
            );
            assert!(
                !state.informed,
                "the licence to prune must not survive a takeover"
            );
        });
    }

    /// The destructive half of a deletion. It erases state only this client
    /// holds — geometry, the label, a remote binding — so the fence in front of
    /// it ("no window is showing this workspace") is the whole safety of it.
    ///
    /// The other half needs a live `Tty7App` in a real window to reach, so it
    /// is not tested here; what it does is hydrate, which the hydration tests
    /// cover, and it touches neither the store nor the registry.
    #[gpui::test]
    fn a_deletion_nothing_has_open_forgets_the_workspace_here_too(cx: &mut gpui::TestAppContext) {
        use crate::core::session::{WindowView, WindowViews};

        cx.update(|cx| {
            // Removing a workspace saves the views, and a test has no business
            // writing the real ones.
            let _ = tty7_core::core::config::set_config_dir(
                std::env::temp_dir().join(format!("tty7-deleted-test-{}", std::process::id())),
            );
            crate::ui::windows::WindowRegistry::init(cx);

            let deleted = WindowView::default();
            let gone = deleted.id;
            let untouched = WindowView::default();
            let survivor = untouched.id;
            WorkspaceStore::install_for_test(
                cx,
                WindowViews {
                    views: vec![deleted, untouched],
                    active: Some(gone),
                },
            );
            cx.default_global::<TreeSync>()
                .windows
                .entry(gone)
                .or_default()
                .sync = SyncPhase::Primed(WsMirror::default());

            on_workspace_deleted(cx, gone);

            assert!(
                WorkspaceStore::all(cx).get(gone).is_none(),
                "a row that opens onto nothing is worse than no row at all"
            );
            assert_eq!(
                WorkspaceStore::all(cx).active,
                None,
                "the active workspace cannot be one that no longer exists"
            );
            assert!(
                WorkspaceStore::all(cx).get(survivor).is_some(),
                "a deletion is about one workspace, not about the store"
            );
            assert!(
                !cx.default_global::<TreeSync>().windows.contains_key(&gone),
                "its sync state has nothing left to be about"
            );
        });
    }

    /// The rule that stops a failed rebuild from being read as "empty".
    ///
    /// A window with no tabs may delete its workspace outright — tree and store
    /// both — so the two ways of having no tabs must not look alike. Genuinely
    /// empty is a reason; "the panes would not start" is not, and it is what
    /// every tab looks like when the pane socket has gone away.
    #[gpui::test]
    fn only_a_mirror_that_agrees_lets_an_empty_window_delete_its_workspace(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(|cx| {
            let ws = WorkspaceId::new();
            let set = |cx: &mut App, informed: bool, sync: SyncPhase| {
                let state = cx
                    .default_global::<TreeSync>()
                    .windows
                    .entry(ws)
                    .or_default();
                state.informed = informed;
                state.sync = sync;
            };
            let unprimed = || SyncPhase::Unprimed {
                dirty: false,
                priming: false,
            };
            let primed_with =
                |tabs: Vec<TreeTab>| SyncPhase::Primed(WsMirror { tabs, active: None });
            let a_tab = || TreeTab {
                id: TabId::new(),
                name: None,
                sidebar_group: None,
                root: PaneNode::Leaf { pane: 1 },
            };

            assert!(
                !workspace_is_disposable(cx, WorkspaceId::new()),
                "a workspace nothing is tracking is not a workspace to delete"
            );

            set(cx, true, unprimed());
            assert!(
                !workspace_is_disposable(cx, ws),
                "an unpulled mirror knows nothing, and not knowing must never authorize this"
            );

            set(cx, true, primed_with(vec![a_tab()]));
            assert!(
                !workspace_is_disposable(cx, ws),
                "this is the regression: the window came up empty because the rebuild failed, \
                 while the machine still held the tabs. Deleting here destroyed them."
            );

            set(cx, false, primed_with(vec![]));
            assert!(
                !workspace_is_disposable(cx, ws),
                "a window that never put up a layout does not get to say what belongs here"
            );

            set(cx, true, primed_with(vec![]));
            assert!(
                workspace_is_disposable(cx, ws),
                "informed, and the machine agrees it holds nothing — the one case that is"
            );
        });
    }

    #[gpui::test]
    fn a_hydration_that_died_on_a_stale_link_is_owed_back(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let ws = WorkspaceId::new();
            let epoch = {
                let state = cx
                    .default_global::<TreeSync>()
                    .windows
                    .entry(ws)
                    .or_default();
                state.sync = SyncPhase::Unprimed {
                    dirty: false,
                    priming: true,
                };
                state.epoch
            };
            owe_rehydration(cx, ws, epoch, Adopt::Replace);
            let state = &cx.default_global::<TreeSync>().windows[&ws];
            assert!(
                matches!(state.sync, SyncPhase::Unprimed { priming: false, .. }),
                "the attempt is over; another one must be able to start"
            );
            assert!(
                state.rehydrate.is_some(),
                "dropping the failure here is what left the window on the home page"
            );

            // A newer attempt has already taken over — the loser must not
            // re-arm a retry behind its back.
            {
                let state = cx
                    .default_global::<TreeSync>()
                    .windows
                    .get_mut(&ws)
                    .unwrap();
                state.rehydrate = None;
                state.epoch += 1;
            }
            owe_rehydration(cx, ws, epoch, Adopt::Replace);
            assert!(
                cx.default_global::<TreeSync>().windows[&ws]
                    .rehydrate
                    .is_none()
            );
        });
    }

    #[gpui::test]
    fn a_window_that_filled_up_while_owed_keeps_what_it_has(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let ws = WorkspaceId::new();
            let arm = |cx: &mut App, adopt| {
                cx.default_global::<TreeSync>()
                    .windows
                    .entry(ws)
                    .or_default()
                    .rehydrate = Some(adopt);
            };

            arm(cx, Adopt::Replace);
            assert!(
                take_rehydrate(cx, ws, true).is_some(),
                "an empty window is exactly the one that still needs its layout"
            );
            assert!(
                take_rehydrate(cx, ws, true).is_none(),
                "the debt is claimed once"
            );

            arm(cx, Adopt::Replace);
            assert!(
                take_rehydrate(cx, ws, false).is_none(),
                "replaying an older layout over the user's new tabs is worse than not retrying"
            );
            assert!(
                cx.default_global::<TreeSync>().windows[&ws]
                    .rehydrate
                    .is_none(),
                "and the dropped retry must not linger"
            );

            arm(cx, Adopt::IfEmpty);
            assert!(
                take_rehydrate(cx, ws, false).is_some(),
                "IfEmpty polices that itself, and still owes the mirror a pull"
            );
        });
    }

    #[test]
    fn a_ratio_delta_is_clamped_to_the_servers_band_not_a_narrower_one() {
        let mut pane = Pane::split_node(gpui::Axis::Horizontal, 0.5, Pane::Empty, Pane::Empty);
        assert!(set_gui_ratio(&mut pane, &[], 0.07));
        match &pane {
            Pane::Split { ratio, .. } => assert_eq!(ratio.get(), 0.07),
            _ => unreachable!("built as a split"),
        }
        assert!(set_gui_ratio(&mut pane, &[], 0.01));
        match &pane {
            Pane::Split { ratio, .. } => assert_eq!(ratio.get(), 0.05),
            _ => unreachable!("built as a split"),
        }
    }

    #[test]
    fn a_tab_created_delta_that_straddled_a_repull_lands_once_in_the_window_mirror() {
        let mut mirror = WsMirror::default();
        let delta = LayoutDelta::TabCreated {
            at: 0,
            tab: TreeTab::leaf(1),
        };
        assert!(apply_to_mirror(&mut mirror, &delta));
        assert!(apply_to_mirror(&mut mirror, &delta));
        assert_eq!(mirror.tabs.len(), 1);
    }

    #[gpui::test]
    fn a_superseded_prime_result_does_not_roll_the_mirror_back(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let ws = WorkspaceId::new();
            let stale_epoch = {
                let state = cx
                    .default_global::<TreeSync>()
                    .windows
                    .entry(ws)
                    .or_default();
                state.sync = SyncPhase::Unprimed {
                    dirty: false,
                    priming: true,
                };
                state.epoch
            };
            let advanced = WsMirror {
                tabs: vec![TreeTab::leaf(7)],
                active: None,
            };
            {
                let state = cx
                    .default_global::<TreeSync>()
                    .windows
                    .get_mut(&ws)
                    .unwrap();
                state.epoch += 1;
                state.sync = SyncPhase::Primed(advanced.clone());
            }

            finish_prime(cx, ws, stale_epoch, Ok(WsMirror::default()));

            match &cx.default_global::<TreeSync>().windows[&ws].sync {
                SyncPhase::Primed(mirror) => assert_eq!(
                    *mirror, advanced,
                    "the stale pull's empty answer must not replace the advanced mirror"
                ),
                _ => panic!("the mirror was dropped entirely"),
            }
        });
    }

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
    fn a_lowered_leaf_carries_its_pane_id_and_its_agent_whatever_live_says() {
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
                    *pane_id,
                    Some(2),
                    "a pane the tree calls dead still goes down by its id: the flag is a \
                     cached observation from another process — reloaded as false on every \
                     server start — and attaching is what settles it. Believing the flag \
                     here spawned fresh shells over running sessions."
                );
                assert_eq!(cwd.as_deref(), Some(std::path::Path::new("/work/api")));
                assert_eq!(*agent, Some(CLIAgent::Claude));
                assert_eq!(agent_session_id.as_deref(), Some("sid"));
            }
            _ => panic!("leaf"),
        }
    }

    /// The regression behind "switching workspaces threw away every session".
    ///
    /// Two servers had started against one config dir — one holding the control
    /// socket with an empty pane registry, the other holding the panes — so
    /// `MachineGet` answered with every `live` still `false`, the value
    /// `load_machine` stamps on a cold read. Erasing the id on that made the
    /// restore spawn a fresh shell over each running one, and nineteen live
    /// agent sessions went out with it.
    ///
    /// The id has to survive a `live: false`, because nothing here is entitled
    /// to declare a pane dead. Attaching is.
    #[test]
    fn a_pane_the_tree_calls_dead_still_goes_down_by_its_id() {
        let tab_id = TabId::new();
        let ws = tty7_core::core::machine::Workspace {
            tabs: vec![TreeTab {
                id: tab_id,
                name: None,
                sidebar_group: None,
                root: PaneNode::Leaf { pane: 7 },
            }],
            active_tab: Some(tab_id),
            ..Default::default()
        };
        let panes = vec![PaneRecord {
            id: 7,
            cwd: Some("/work".into()),
            live: false,
            ..PaneRecord::new(7)
        }];

        match &session_from_tree(&ws, &panes).tabs[0].pane {
            SessionPane::Leaf { pane_id, .. } => assert_eq!(
                *pane_id,
                Some(7),
                "the attach decides whether pane 7 is still there; this must not pre-empt it"
            ),
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

        let want = vec![tab(a, leaf(2)), tab(b, leaf(2))];
        let ops = diff(ws, &mut mirror, &want, Some(b), SyncScope::Full, &[]);
        assert!(
            !ops.iter()
                .any(|op| matches!(op, ControlRequest::PaneReplace { .. })),
            "got {ops:?}"
        );
    }
}
