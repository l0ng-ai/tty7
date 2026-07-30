//! A per-machine mirror of each daemon's workspace tree, for the surfaces that
//! read *about* workspaces without showing them.
//!
//! The machine's tree is the layout authority, so anything the client used to
//! answer from its own saved layout — a picker row's name, the "3 panes"
//! count, which pane ids a workspace claims — has to come from the tree now.
//! The windows that *show* a workspace already hold a per-window mirror
//! ([`crate::ui::tree_sync`]); this global is the read model for everything
//! else: the switcher, the Window menu, the title bar, the liveness sweep.
//!
//! # How it stays current
//!
//! One [`Machine`] per [`HostId`], filled by a `MachineGet` when a machine's
//! control link comes up and advanced from there by the same
//! [`LayoutDelta`] stream the windows consume — plus
//! [`note_synced_workspace`], because origin exclusion means this client never
//! hears its **own** operations back, and the per-window mirror they advanced
//! is the only other record of what they did.
//!
//! A delta that will not apply (a machine the pull has not answered for yet, a
//! tab it never heard of) marks nothing broken: the mirror re-pulls the whole
//! machine, exactly like a drifted window does.
//!
//! # It may be behind, and that is allowed
//!
//! Against the local daemon the first pull lands within milliseconds of
//! launch, so the picker's loading gap is about one frame. A machine that is
//! unreachable keeps its last pulled state for the rest of the process — stale
//! names beat no names — and a machine never reached this session simply has
//! no entry, which readers render as the not-knowing they are in.

use std::collections::HashMap;

use gpui::{App, Global};
use tty7_core::core::machine::{LayoutDelta, Machine, PaneRecord, Tab, TabId, Workspace};
use tty7_core::daemon::control::{ControlRequest, ReplyOk};
use tty7_core::host::HostId;

use crate::core::session::WorkspaceId;

/// Every machine's last known tree, by the machine.
#[derive(Default)]
pub struct MachineMirrors {
    machines: HashMap<HostId, Machine>,
    /// Hosts with a `MachineGet` in flight, so a burst of triggers costs one
    /// round trip.
    pulling: Vec<HostId>,
}

impl Global for MachineMirrors {}

impl MachineMirrors {
    /// The last pulled tree for `host`, or `None` when no pull has answered
    /// yet this session. Read-only; renders may call it every frame.
    pub fn machine(cx: &App, host: HostId) -> Option<&Machine> {
        cx.try_global::<Self>()?.machines.get(&host)
    }

    /// Whether `host`'s tree has been pulled at all — the "loading" /
    /// "known but empty" distinction a picker wants to draw.
    pub fn ready(cx: &App, host: HostId) -> bool {
        Self::machine(cx, host).is_some()
    }

    /// Pull `host`'s whole tree in the background and install it. Cheap to
    /// call whenever a link comes up or a delta refuses to apply; concurrent
    /// triggers coalesce into one round trip.
    pub fn refresh(cx: &mut App, host: HostId) {
        // A peer that does not advertise `machine-tree` (a server with no
        // home directory for one) has no tree to pull; asking anyway costs a
        // round trip per trigger to hear the same refusal. Reads keep their
        // "never pulled" answer, which renders as not knowing.
        let client = match crate::ui::tree_sync::tree_control_for(cx, host) {
            crate::ui::tree_sync::TreeLink::Ready(client) => client,
            crate::ui::tree_sync::TreeLink::Unserved => {
                log::debug!("not pulling {host:?}: its server does not serve the machine tree");
                return;
            }
            crate::ui::tree_sync::TreeLink::Down => return,
        };
        let mirrors = cx.default_global::<Self>();
        if mirrors.pulling.contains(&host) {
            return;
        }
        mirrors.pulling.push(host);
        cx.spawn(async move |cx| {
            let pulled = cx
                .background_executor()
                .spawn(async move {
                    match client.call(ControlRequest::MachineGet) {
                        Ok(ReplyOk::MachineTree(machine)) => Some(machine),
                        Ok(other) => {
                            log::warn!("MachineGet answered {other:?}");
                            None
                        }
                        Err(e) => {
                            log::debug!("could not pull the machine tree: {e}");
                            None
                        }
                    }
                })
                .await;
            cx.update(|cx| {
                let mirrors = cx.default_global::<Self>();
                mirrors.pulling.retain(|h| *h != host);
                if let Some(machine) = pulled {
                    mirrors.machines.insert(host, *machine);
                    cx.refresh_windows();
                }
            });
        })
        .detach();
    }

    /// Install a freshly pulled tree — for the paths that already hold one
    /// (a window's hydration pulls `MachineGet` anyway).
    ///
    /// Repaints, like [`refresh`](Self::refresh)'s landing does: every workspace
    /// name, pane count and liveness dot on screen reads this global, and a
    /// pull that lands without a repaint leaves the chrome a frame (or, on a
    /// quiet screen, indefinitely) behind the tree it is describing.
    pub fn install(cx: &mut App, host: HostId, machine: Machine) {
        cx.default_global::<Self>().machines.insert(host, machine);
        cx.refresh_windows();
    }

    /// Advance `host`'s mirror by one delta about the workspace `key` names.
    ///
    /// A delta that names state the mirror does not hold re-pulls the machine
    /// whole; a delta arriving before the first pull is dropped, because that
    /// pull's answer already includes it.
    pub fn apply_delta(cx: &mut App, host: HostId, key: &str, delta: &LayoutDelta) {
        let Ok(id) = key.parse::<WorkspaceId>() else {
            return;
        };
        let applied = match cx.default_global::<Self>().machines.get_mut(&host) {
            Some(machine) => apply(machine, id, delta),
            None => true,
        };
        if !applied {
            log::debug!("machine mirror for {host:?} fell behind; re-pulling");
            Self::refresh(cx, host);
        }
    }

    /// Record the post-state of this client's own operations on `machine_ws` —
    /// the half of the history origin exclusion keeps out of the delta stream.
    /// A workspace the mirror has not seen is created; `None` tabs leave the
    /// structure alone (a label-only op).
    pub fn note_synced_workspace(
        cx: &mut App,
        host: HostId,
        machine_ws: WorkspaceId,
        tabs: Vec<Tab>,
        active: Option<TabId>,
    ) {
        let Some(machine) = cx.default_global::<Self>().machines.get_mut(&host) else {
            return;
        };
        let ws = match machine.workspaces.iter_mut().find(|w| w.id == machine_ws) {
            Some(ws) => ws,
            None => {
                machine.workspaces.push(Workspace {
                    id: machine_ws,
                    ..Workspace::default()
                });
                machine.workspaces.last_mut().expect("just pushed")
            }
        };
        ws.tabs = tabs;
        ws.active_tab = active;
    }

    /// Fold in a workspace-level operation this client just fired
    /// ([`crate::ui::tree_sync::fire_workspace_op`]) — same reason as
    /// [`note_synced_workspace`]: the writer never hears its own echo.
    pub fn note_workspace_op(cx: &mut App, host: HostId, request: &ControlRequest) {
        let Some(machine) = cx.default_global::<Self>().machines.get_mut(&host) else {
            return;
        };
        match request {
            ControlRequest::WorkspaceRename { workspace, name } => {
                if let Some(ws) = machine.workspaces.iter_mut().find(|w| w.id == *workspace) {
                    ws.name = name.clone();
                }
            }
            ControlRequest::WorkspaceTouch { workspace } => {
                if let Some(ws) = machine.workspaces.iter_mut().find(|w| w.id == *workspace) {
                    ws.last_active = crate::ui::home::now_secs();
                }
            }
            ControlRequest::WorkspaceRemove { workspace } => {
                machine.workspaces.retain(|w| w.id != *workspace);
            }
            _ => {}
        }
    }
}

/// Advance one machine's copy by one delta. `false` means the delta names
/// state the mirror does not hold and the caller should re-pull.
fn apply(machine: &mut Machine, workspace: WorkspaceId, delta: &LayoutDelta) -> bool {
    // The two deltas that do not require the workspace to exist yet.
    match delta {
        LayoutDelta::WorkspaceCreated { workspace: ws } => {
            machine.workspaces.retain(|w| w.id != ws.id);
            machine.workspaces.push(ws.clone());
            return true;
        }
        LayoutDelta::WorkspaceDeleted => {
            machine.workspaces.retain(|w| w.id != workspace);
            return true;
        }
        // Facts about a pane are registry-wide; the workspace key only says
        // who referenced it. Upserted rather than matched, because the record
        // may have been born from another client's op this mirror never saw.
        LayoutDelta::PaneFacts { pane } => {
            match machine.panes.iter_mut().find(|p| p.id == pane.id) {
                Some(record) => *record = pane.clone(),
                None => machine.panes.push(pane.clone()),
            }
            return true;
        }
        _ => {}
    }
    let Some(ws) = machine.workspaces.iter_mut().find(|w| w.id == workspace) else {
        return false;
    };
    match delta {
        LayoutDelta::WorkspaceCreated { .. }
        | LayoutDelta::WorkspaceDeleted
        | LayoutDelta::PaneFacts { .. } => unreachable!("handled above"),
        LayoutDelta::WorkspaceRenamed { name } => {
            ws.name = name.clone();
            true
        }
        LayoutDelta::WorkspaceTouched { last_active } => {
            ws.last_active = *last_active;
            true
        }
        LayoutDelta::ActiveTabChanged { tab } => {
            ws.active_tab = Some(*tab);
            true
        }
        LayoutDelta::TabCreated { at, tab } => {
            // Deltas and full pulls have no ordering barrier: a create that
            // straddles a pull arrives *after* the snapshot that already
            // carries its tab. Replace-by-id (the `WorkspaceCreated` retain
            // above is the precedent) rather than insert twice.
            ws.tabs.retain(|t| t.id != tab.id);
            let at = (*at).min(ws.tabs.len());
            ws.tabs.insert(at, tab.clone());
            true
        }
        LayoutDelta::TabClosed { tab } => {
            let before = ws.tabs.len();
            ws.tabs.retain(|t| t.id != *tab);
            if ws.tabs.is_empty() {
                ws.active_tab = None;
            }
            ws.tabs.len() != before
        }
        LayoutDelta::TabRenamed { tab, name } => {
            let Some(t) = ws.tabs.iter_mut().find(|t| t.id == *tab) else {
                return false;
            };
            t.name = name.clone();
            true
        }
        LayoutDelta::TabRegrouped { tab, group } => {
            let Some(t) = ws.tabs.iter_mut().find(|t| t.id == *tab) else {
                return false;
            };
            t.sidebar_group = group.clone();
            true
        }
        LayoutDelta::TabMoved { tab, to } => {
            let Some(from) = ws.tabs.iter().position(|t| t.id == *tab) else {
                return false;
            };
            let moved = ws.tabs.remove(from);
            ws.tabs.insert((*to).min(ws.tabs.len()), moved);
            true
        }
        LayoutDelta::TabRestructured { tab, pane } => {
            let Some(t) = ws.tabs.iter_mut().find(|t| t.id == tab.id) else {
                return false;
            };
            *t = tab.clone();
            if let Some(pane) = pane {
                match machine.panes.iter_mut().find(|p| p.id == pane.id) {
                    Some(record) => *record = pane.clone(),
                    None => machine.panes.push(pane.clone()),
                }
            }
            true
        }
        LayoutDelta::RatioChanged { tab, path, ratio } => {
            let Some(t) = ws.tabs.iter_mut().find(|t| t.id == *tab) else {
                return false;
            };
            match t.root.descend_mut(path) {
                Some(tty7_core::core::machine::PaneNode::Split { ratio: r, .. }) => {
                    *r = *ratio;
                    true
                }
                _ => false,
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Reading a client entry's display facts off its machine's mirror
// ---------------------------------------------------------------------------

/// The tree workspace a client entry points at, with the pane registry it
/// reads records from. `None` while the machine has not been pulled (or no
/// longer lists the workspace).
fn view_of<'a>(
    cx: &'a App,
    entry: &crate::core::session::WindowView,
) -> Option<(&'a Workspace, &'a [PaneRecord])> {
    let machine = MachineMirrors::machine(cx, entry.host_id())?;
    let machine_ws = entry.host.as_ref().map(|r| r.workspace).unwrap_or(entry.id);
    let ws = machine.workspaces.iter().find(|w| w.id == machine_ws)?;
    Some((ws, &machine.panes))
}

/// What the picker and the window title call `entry`: the user-set name, else
/// derived from the tree's repo groups and cwds.
///
/// Falls back to
/// [`WindowView::label`](crate::core::session::WindowView::label) — what the
/// machine last said, before it
/// stopped answering. The tree wins whenever it answers; the hint is for the
/// rows the picker exists to offer, on machines that are asleep. `None` only
/// when this client has never seen the workspace named at all, which is a
/// brand-new entry and nothing a user is choosing between.
pub fn display_name(cx: &App, entry: &crate::core::session::WindowView) -> Option<String> {
    match view_of(cx, entry) {
        Some((ws, panes)) => Some(display_name_of(ws, panes)),
        None => entry.label.clone(),
    }
}

/// A tree workspace's label: the user-set name, else the repository most of
/// its tabs live in, else the first pane's directory, else `"Untitled"`.
pub fn display_name_of(ws: &Workspace, panes: &[PaneRecord]) -> String {
    if let Some(name) = ws.name.as_deref().map(str::trim).filter(|n| !n.is_empty()) {
        return name.to_string();
    }
    subject_path_of(ws, panes)
        .and_then(|path| {
            std::path::Path::new(&path)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
        })
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "Untitled".to_string())
}

/// The path a workspace is *about*: the repo group most tabs belong to (ties
/// toward the earliest tab), else the first pane's cwd. What the picker's dim
/// subtitle shows, and what [`display_name_of`] takes the basename of.
pub fn subject_path_of(ws: &Workspace, panes: &[PaneRecord]) -> Option<String> {
    let mut counts: Vec<(&str, usize)> = Vec::new();
    for group in ws.tabs.iter().filter_map(|t| t.sidebar_group.as_deref()) {
        match counts.iter_mut().find(|(g, _)| *g == group) {
            Some((_, n)) => *n += 1,
            None => counts.push((group, 1)),
        }
    }
    let dominant = counts.into_iter().max_by_key(|(_, n)| *n).map(|(g, _)| g);
    let first_cwd = ws
        .tabs
        .iter()
        .flat_map(|t| t.root.pane_ids())
        .find_map(|id| {
            panes
                .iter()
                .find(|p| p.id == id)
                .and_then(|p| p.cwd.as_deref())
        });
    dominant.or(first_cwd).map(str::to_string)
}

/// [`display_name`] looked up by the client's workspace id, with the shared
/// not-knowing fallback — for the sites that hold an id rather than an entry.
pub fn display_name_for(cx: &App, client_ws: WorkspaceId) -> Option<String> {
    let entry = crate::core::session::WorkspaceStore::all(cx).get(client_ws)?;
    display_name(cx, entry)
}

/// [`subject_path_of`] for a client entry, falling back to the stamped hint for
/// the same reason [`display_name`] does.
pub fn subject_path(cx: &App, entry: &crate::core::session::WindowView) -> Option<String> {
    match view_of(cx, entry) {
        Some((ws, panes)) => subject_path_of(ws, panes).or_else(|| entry.subject.clone()),
        None => entry.subject.clone(),
    }
}

/// The pair a client entry should carry on file, read off its machine's mirror —
/// for [`WorkspaceStore::record_geometry`](crate::core::session::WorkspaceStore::record_geometry)
/// to stamp. `None` for a machine that has not answered: a hint is only ever
/// replaced by something better, never blanked by not knowing.
pub fn display_hint(
    cx: &App,
    entry: &crate::core::session::WindowView,
) -> Option<(String, Option<String>)> {
    let (ws, panes) = view_of(cx, entry)?;
    Some((display_name_of(ws, panes), subject_path_of(ws, panes)))
}

/// Every pane id `entry`'s tree claims on its machine. `None` when the
/// machine's tree has not been pulled — which a caller about to state a fact
/// ("3 running sessions will be ended") must render as not knowing, not as
/// zero.
pub fn pane_ids(cx: &App, entry: &crate::core::session::WindowView) -> Option<Vec<u64>> {
    let (ws, _) = match view_of(cx, entry) {
        Some(view) => view,
        // A pulled machine that no longer lists the workspace *is* an answer:
        // it claims nothing.
        None if MachineMirrors::ready(cx, entry.host_id()) => return Some(Vec::new()),
        None => return None,
    };
    Some(ws.tabs.iter().flat_map(|t| t.root.pane_ids()).collect())
}

/// How many terminals `entry` holds across every tab, per its machine's tree.
pub fn pane_count(cx: &App, entry: &crate::core::session::WindowView) -> Option<usize> {
    pane_ids(cx, entry).map(|ids| ids.len())
}

#[cfg(test)]
mod tests {
    use tty7_core::core::machine::{Axis, PaneNode, Tab, TabId};

    use super::*;

    fn machine_with(ws: Workspace) -> Machine {
        Machine {
            workspaces: vec![ws],
            panes: Vec::new(),
        }
    }

    fn leaf_tab(pane: u64) -> Tab {
        Tab::leaf(pane)
    }

    /// A machine that is not answering still has to produce a row a user can
    /// choose: the picker's whole job is offering workspaces on machines that
    /// are asleep, and "Untitled" with a blank subtitle is not an offer. So the
    /// stamped hint stands in until a pull lands, and the tree wins the moment
    /// one does.
    #[gpui::test]
    fn an_unpulled_machine_falls_back_to_the_stamped_label(cx: &mut gpui::TestAppContext) {
        use crate::core::session::{WindowView, WindowViews, WorkspaceStore};

        cx.update(|cx| {
            let mut view = WindowView::default();
            view.label = Some("api".into());
            view.subject = Some("/repo/api".into());
            let id = view.id;
            let entry = view.clone();
            WorkspaceStore::install_for_test(
                cx,
                WindowViews {
                    views: vec![view],
                    active: None,
                },
            );

            // Nothing pulled: the hint is what the row says.
            assert_eq!(display_name(cx, &entry).as_deref(), Some("api"));
            assert_eq!(subject_path(cx, &entry).as_deref(), Some("/repo/api"));
            assert!(
                display_hint(cx, &entry).is_none(),
                "and a machine that has not answered contributes no new hint"
            );

            // The tree answers, and outranks it.
            let mut tree = Workspace {
                id,
                name: Some("web".into()),
                ..Workspace::default()
            };
            tree.tabs = vec![leaf_tab(1)];
            MachineMirrors::install(cx, HostId::LOCAL, machine_with(tree));
            assert_eq!(display_name(cx, &entry).as_deref(), Some("web"));
            assert_eq!(
                display_hint(cx, &entry).map(|(label, _)| label).as_deref(),
                Some("web"),
                "which is what the next save stamps"
            );
        });
    }

    #[test]
    fn a_workspace_created_delta_lands_whole_and_a_deleted_one_removes_it() {
        let mut machine = Machine::default();
        let ws = Workspace::default();
        let id = ws.id;
        assert!(apply(
            &mut machine,
            id,
            &LayoutDelta::WorkspaceCreated { workspace: ws },
        ));
        assert_eq!(machine.workspaces.len(), 1);
        assert!(apply(&mut machine, id, &LayoutDelta::WorkspaceDeleted));
        assert!(machine.workspaces.is_empty());
    }

    #[test]
    fn structural_deltas_advance_the_mirrored_tree() {
        let ws = Workspace::default();
        let id = ws.id;
        let mut machine = machine_with(ws);
        let tab = leaf_tab(1);
        let tab_id = tab.id;
        assert!(apply(
            &mut machine,
            id,
            &LayoutDelta::TabCreated { at: 0, tab },
        ));
        let restructured = Tab {
            id: tab_id,
            name: None,
            sidebar_group: None,
            root: PaneNode::Split {
                axis: Axis::Vertical,
                ratio: 0.5,
                a: Box::new(PaneNode::Leaf { pane: 1 }),
                b: Box::new(PaneNode::Leaf { pane: 2 }),
            },
        };
        assert!(apply(
            &mut machine,
            id,
            &LayoutDelta::TabRestructured {
                tab: restructured,
                pane: Some(PaneRecord::new(2)),
            },
        ));
        let ws = &machine.workspaces[0];
        assert_eq!(ws.tabs[0].root.pane_ids(), vec![1, 2]);
        assert_eq!(
            machine.panes.len(),
            1,
            "the rider pane record is upserted into the registry"
        );
    }

    /// Deltas and full pulls have no ordering barrier: a `TabCreated` that
    /// straddles a `MachineGet` arrives after a snapshot that already carries
    /// its tab. Applying it must replace by id, not insert a second copy.
    #[test]
    fn a_tab_created_delta_that_straddled_a_pull_lands_once() {
        let ws = Workspace::default();
        let id = ws.id;
        let mut machine = machine_with(ws);
        let delta = LayoutDelta::TabCreated {
            at: 0,
            tab: leaf_tab(1),
        };
        assert!(apply(&mut machine, id, &delta));
        assert!(apply(&mut machine, id, &delta));
        assert_eq!(
            machine.workspaces[0].tabs.len(),
            1,
            "the second application is the pull/delta overlap, not a second tab"
        );
    }

    #[test]
    fn a_delta_about_a_tab_the_mirror_never_saw_asks_for_a_repull() {
        let ws = Workspace::default();
        let id = ws.id;
        let mut machine = machine_with(ws);
        assert!(
            !apply(
                &mut machine,
                id,
                &LayoutDelta::TabRenamed {
                    tab: TabId::new(),
                    name: Some("x".into()),
                },
            ),
            "an unappliable delta must say so, so the caller re-pulls"
        );
        // …and so does one about a workspace the machine does not list.
        assert!(!apply(
            &mut machine,
            WorkspaceId::new(),
            &LayoutDelta::WorkspaceRenamed { name: None },
        ));
    }

    #[test]
    fn pane_facts_upsert_the_registry_even_for_a_pane_born_elsewhere() {
        let mut machine = Machine::default();
        let mut record = PaneRecord::new(7);
        record.cwd = Some("/work".into());
        assert!(apply(
            &mut machine,
            WorkspaceId::new(),
            &LayoutDelta::PaneFacts {
                pane: record.clone(),
            },
        ));
        record.live = true;
        assert!(apply(
            &mut machine,
            WorkspaceId::new(),
            &LayoutDelta::PaneFacts { pane: record },
        ));
        assert_eq!(machine.panes.len(), 1, "updated in place, not duplicated");
        assert!(machine.panes[0].live);
    }

    /// The precedence `Workspace::display_name` always had, read off the tree:
    /// user name, then the dominant repo group, then the first pane's cwd.
    #[test]
    fn display_names_derive_from_the_tree_with_the_session_precedence() {
        let mut ws = Workspace::default();
        let panes = vec![PaneRecord {
            cwd: Some("/home/me/scratch".into()),
            ..PaneRecord::new(1)
        }];
        ws.tabs = vec![leaf_tab(1)];
        assert_eq!(display_name_of(&ws, &panes), "scratch");

        ws.tabs[0].sidebar_group = Some("/repo/tty7".into());
        assert_eq!(display_name_of(&ws, &panes), "tty7");

        ws.name = Some("  Release prep  ".into());
        assert_eq!(display_name_of(&ws, &panes), "Release prep");

        assert_eq!(display_name_of(&Workspace::default(), &[]), "Untitled");
    }
}
