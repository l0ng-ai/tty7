use std::collections::HashMap;

use gpui::{App, Global};
use tty7_core::core::codename;
use tty7_core::core::machine::{LayoutDelta, Machine, PaneRecord, Tab, TabId, Workspace};
use tty7_core::daemon::control::{ControlRequest, ReplyOk};
use tty7_core::host::HostId;

use crate::core::session::WorkspaceId;
use crate::ui::i18n::{L10nKey, t};

#[derive(Default)]
pub struct MachineMirrors {
    machines: HashMap<HostId, Machine>,
    pulling: Vec<HostId>,
}

impl Global for MachineMirrors {}

impl MachineMirrors {
    pub fn machine(cx: &App, host: HostId) -> Option<&Machine> {
        cx.try_global::<Self>()?.machines.get(&host)
    }

    pub fn ready(cx: &App, host: HostId) -> bool {
        Self::machine(cx, host).is_some()
    }

    pub fn refresh(cx: &mut App, host: HostId) {
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

    pub fn install(cx: &mut App, host: HostId, machine: Machine) {
        cx.default_global::<Self>().machines.insert(host, machine);
        cx.refresh_windows();
    }

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

fn apply(machine: &mut Machine, workspace: WorkspaceId, delta: &LayoutDelta) -> bool {
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

fn view_of<'a>(
    cx: &'a App,
    entry: &crate::core::session::WindowView,
) -> Option<(&'a Workspace, &'a [PaneRecord])> {
    let machine = MachineMirrors::machine(cx, entry.host_id())?;
    let machine_ws = entry.host.as_ref().map(|r| r.workspace).unwrap_or(entry.id);
    let ws = machine.workspaces.iter().find(|w| w.id == machine_ws)?;
    Some((ws, &machine.panes))
}

pub fn display_name(cx: &App, entry: &crate::core::session::WindowView) -> Option<String> {
    let Some((ws, _)) = view_of(cx, entry) else {
        return entry.label.clone();
    };
    // `subject_path` rather than `subject_path_of`: the entry remembers the
    // directory from when the tree could still say, which is what a rebuilt
    // window has to read it from (#604, see `subject_path`).
    Some(name_from(
        ws.name.as_deref(),
        subject_path(cx, entry).as_deref(),
    ))
}

pub fn display_name_of(ws: &Workspace, panes: &[PaneRecord]) -> String {
    name_from(ws.name.as_deref(), subject_path_of(ws, panes).as_deref())
}

/// What to call a workspace, from its name and the directory its shells are
/// working in.
///
/// A name somebody chose wins outright. A codename does not: every workspace a
/// client creates is handed one so the tree and the CLI have something to call
/// it, and the window that asked for it is never told which one it got — its
/// mirror carries the workspace unnamed until the tree is pulled whole again.
/// Letting a name nobody chose outrank the directory meant that pull renamed
/// the workspace on screen, and a daemon restart is only one of the things
/// that pulls (#604).
fn name_from(name: Option<&str>, subject: Option<&str>) -> String {
    let named = name.map(str::trim).filter(|n| !n.is_empty());
    if let Some(chosen) = named.filter(|n| !codename::is_generated(n)) {
        return chosen.to_string();
    }
    subject
        .and_then(|path| {
            std::path::Path::new(path)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
        })
        .filter(|s| !s.is_empty())
        // No directory to borrow from — an empty workspace, or one nobody has
        // ever recorded a cwd for. A codename still beats "Untitled" at telling
        // two of those apart in a list, which is what it was minted for.
        .or_else(|| named.map(str::to_string))
        .unwrap_or_else(|| t(L10nKey::WindowUntitled).to_string())
}

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

pub fn display_name_for(cx: &App, client_ws: WorkspaceId) -> Option<String> {
    let entry = crate::core::session::WorkspaceStore::all(cx).get(client_ws)?;
    display_name(cx, entry)
}

/// One tab of some workspace, flattened down to what a list row needs. The
/// mirror is the only place that knows about workspaces this window does not
/// own, so the switcher's tab column reads them from here — and so does
/// `tty7 tab ls`, which is why the reading itself lives in the core.
pub use tty7_core::core::tab_view::{TabLabel, TabView, tab_views_of};

pub fn tab_views_for(cx: &App, client_ws: WorkspaceId) -> Option<(Vec<TabView>, Option<TabId>)> {
    let (ws, panes) = match crate::core::session::WorkspaceStore::all(cx).get(client_ws) {
        Some(entry) => view_of(cx, entry)?,
        // Not in the store: a workspace some other client made, which this
        // window has never opened. It can only be on this machine, and its
        // tree id is the id we were handed.
        None => local_view_of(cx, client_ws)?,
    };
    Some((tab_views_of(ws, panes), ws.active_tab))
}

fn local_view_of(cx: &App, id: WorkspaceId) -> Option<(&Workspace, &[PaneRecord])> {
    let machine = MachineMirrors::machine(cx, HostId::LOCAL)?;
    let ws = machine.workspaces.iter().find(|w| w.id == id)?;
    Some((ws, &machine.panes))
}

/// Does this machine hold a workspace by this id, with tabs in it? A window
/// opening one has to pull those tabs in: starting empty and saving the empty
/// session back would erase them.
pub fn machine_holds_tabs(cx: &App, id: WorkspaceId) -> bool {
    local_view_of(cx, id).is_some_and(|(ws, _)| !ws.tabs.is_empty())
}

/// A workspace this machine holds that the local store has never heard of.
pub struct UnclaimedWorkspace {
    pub id: WorkspaceId,
    pub name: String,
    pub path: Option<String>,
    pub last_active: u64,
    pub live: bool,
}

/// Workspaces made by the CLI, or by another client — as real as any other,
/// the only thing they lack is a window here. The switcher lists them so that
/// `tty7 new` does not look like it did nothing.
pub fn unclaimed_local_workspaces(cx: &App) -> Vec<UnclaimedWorkspace> {
    let Some(machine) = MachineMirrors::machine(cx, HostId::LOCAL) else {
        return Vec::new();
    };
    let views = crate::core::session::WorkspaceStore::all(cx);
    machine
        .workspaces
        .iter()
        .filter(|ws| views.get(ws.id).is_none())
        .map(|ws| UnclaimedWorkspace {
            id: ws.id,
            name: display_name_of(ws, &machine.panes),
            path: subject_path_of(ws, &machine.panes),
            last_active: ws.last_active,
            live: ws
                .tabs
                .iter()
                .flat_map(|t| t.root.pane_ids())
                .filter_map(|id| machine.panes.iter().find(|p| p.id == id))
                .any(|p| p.live),
        })
        .collect()
}

/// Where this workspace is working, as the tree has it — falling back to what
/// the entry remembers.
///
/// The fallback is not only for a machine that has not answered yet. A window
/// is left out of the deltas its own edits raise, and the pane records ride
/// along inside those deltas, so a pane this window created is in its mirror's
/// tabs while the mirror holds no record for it — no cwd, no directory. The
/// daemon closes the gap only when some fact later *changes*, and a pane
/// restored with the cwd it will keep changes nothing. So after a rebuild the
/// tree this window reads has forgotten where it is, and the entry is the only
/// one still holding it (#604).
pub fn subject_path(cx: &App, entry: &crate::core::session::WindowView) -> Option<String> {
    match view_of(cx, entry) {
        Some((ws, panes)) => subject_path_of(ws, panes).or_else(|| entry.subject.clone()),
        None => entry.subject.clone(),
    }
}

pub fn display_hint(
    cx: &App,
    entry: &crate::core::session::WindowView,
) -> Option<(String, Option<String>)> {
    let (ws, _) = view_of(cx, entry)?;
    // Both halves read through the fallback, so the save this feeds writes the
    // directory back rather than dropping it the first time the mirror cannot
    // name one (#604).
    let subject = subject_path(cx, entry);
    Some((name_from(ws.name.as_deref(), subject.as_deref()), subject))
}

pub fn pane_ids(cx: &App, entry: &crate::core::session::WindowView) -> Option<Vec<u64>> {
    let (ws, _) = match view_of(cx, entry) {
        Some(view) => view,
        None if MachineMirrors::ready(cx, entry.host_id()) => return Some(Vec::new()),
        None => return None,
    };
    Some(ws.tabs.iter().flat_map(|t| t.root.pane_ids()).collect())
}

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

            assert_eq!(display_name(cx, &entry).as_deref(), Some("api"));
            assert_eq!(subject_path(cx, &entry).as_deref(), Some("/repo/api"));
            assert!(
                display_hint(cx, &entry).is_none(),
                "and a machine that has not answered contributes no new hint"
            );

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

    /// #604: a window creates its workspace with a codename it is never told,
    /// so its mirror carries the workspace unnamed and the chip reads the
    /// directory. Pulling the tree whole — which a daemon restart, a rebuild
    /// and a plain relaunch all do — used to hand that codename to the chip and
    /// stamp it into `views.json` from there, and the rebuild had by then also
    /// cost the mirror the pane record the directory was read from.
    #[gpui::test]
    fn a_rebuild_does_not_rename_a_workspace_to_its_codename(cx: &mut gpui::TestAppContext) {
        use crate::core::session::{WindowView, WindowViews, WorkspaceStore};

        cx.update(|cx| {
            let mut entry = WindowView::default();
            let id = entry.id;
            WorkspaceStore::install_for_test(
                cx,
                WindowViews {
                    views: vec![entry.clone()],
                    active: None,
                },
            );

            let holding = |pane: u64| Workspace {
                id,
                tabs: vec![leaf_tab(pane)],
                ..Workspace::default()
            };
            let working_in = |pane: u64| {
                vec![PaneRecord {
                    cwd: Some("/work/verify-main".into()),
                    ..PaneRecord::new(pane)
                }]
            };

            // As the window that created the workspace sees it: the codename it
            // asked for came back in no delta it was sent.
            MachineMirrors::install(
                cx,
                HostId::LOCAL,
                Machine {
                    workspaces: vec![holding(1)],
                    panes: working_in(1),
                },
            );
            assert_eq!(display_name(cx, &entry).as_deref(), Some("verify-main"));
            let (label, subject) = display_hint(cx, &entry).expect("the machine has answered");
            assert_eq!(label, "verify-main");
            assert_eq!(subject.as_deref(), Some("/work/verify-main"));
            // What the next save stamps.
            entry.label = Some(label);
            entry.subject = subject;

            // The rebuild: the tree as the machine really holds it, codename and
            // all, and the pane on screen is one this window restored — in the
            // workspace's tabs, in no pane record the window was sent.
            MachineMirrors::install(
                cx,
                HostId::LOCAL,
                Machine {
                    workspaces: vec![Workspace {
                        name: Some("keen-marten".into()),
                        ..holding(2)
                    }],
                    panes: Vec::new(),
                },
            );
            assert_eq!(
                display_name(cx, &entry).as_deref(),
                Some("verify-main"),
                "a name nobody chose does not outrank the directory"
            );
            assert_eq!(
                display_hint(cx, &entry),
                Some(("verify-main".to_string(), Some("/work/verify-main".into()))),
                "and the save that follows writes that back rather than the codename"
            );

            // A name the user did choose wins, rebuild or no rebuild.
            MachineMirrors::install(
                cx,
                HostId::LOCAL,
                Machine {
                    workspaces: vec![Workspace {
                        name: Some("deploy".into()),
                        ..holding(2)
                    }],
                    panes: Vec::new(),
                },
            );
            assert_eq!(display_name(cx, &entry).as_deref(), Some("deploy"));

            // With no directory anywhere to borrow from, the codename is still
            // better than "Untitled" — that is the list it was minted for.
            entry.subject = None;
            MachineMirrors::install(
                cx,
                HostId::LOCAL,
                Machine {
                    workspaces: vec![Workspace {
                        name: Some("keen-marten".into()),
                        ..holding(2)
                    }],
                    panes: Vec::new(),
                },
            );
            assert_eq!(display_name(cx, &entry).as_deref(), Some("keen-marten"));
        });
    }

    #[gpui::test]
    fn a_workspace_the_store_never_saw_is_still_listed_and_still_readable(
        cx: &mut gpui::TestAppContext,
    ) {
        use crate::core::session::{WindowView, WindowViews, WorkspaceStore};

        cx.update(|cx| {
            let mine = WindowView::default();
            let known = mine.id;
            WorkspaceStore::install_for_test(
                cx,
                WindowViews {
                    views: vec![mine],
                    active: None,
                },
            );

            // What `tty7 new` leaves behind: on the machine, named, with a
            // tab — and no window here has ever heard of it.
            let theirs = Workspace {
                name: Some("demo".into()),
                tabs: vec![leaf_tab(7)],
                ..Workspace::default()
            };
            let cli_made = theirs.id;
            MachineMirrors::install(
                cx,
                HostId::LOCAL,
                Machine {
                    workspaces: vec![
                        Workspace {
                            id: known,
                            tabs: vec![leaf_tab(1)],
                            ..Workspace::default()
                        },
                        theirs,
                    ],
                    panes: vec![PaneRecord {
                        cwd: Some("/repo/demo".into()),
                        live: true,
                        ..PaneRecord::new(7)
                    }],
                },
            );

            let unclaimed = unclaimed_local_workspaces(cx);
            assert_eq!(unclaimed.len(), 1, "the store's own workspace is not new");
            assert_eq!(unclaimed[0].id, cli_made);
            assert_eq!(unclaimed[0].name, "demo");
            assert_eq!(unclaimed[0].path.as_deref(), Some("/repo/demo"));
            assert!(unclaimed[0].live);

            assert!(
                machine_holds_tabs(cx, cli_made),
                "opening it has to pull those tabs, not save an empty session over them"
            );
            let (tabs, _) = tab_views_for(cx, cli_made).expect("readable without a store entry");
            assert_eq!(tabs.len(), 1);
            assert_eq!(tabs[0].cwd.as_deref(), Some("/repo/demo"));

            assert!(!machine_holds_tabs(cx, WorkspaceId::new()));
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

    #[test]
    fn display_names_derive_from_the_tree_with_the_session_precedence() {
        let mut ws = Workspace::default();
        let mut panes = vec![PaneRecord {
            cwd: Some("/home/me/scratch".into()),
            ..PaneRecord::new(1)
        }];
        ws.tabs = vec![leaf_tab(1)];
        assert_eq!(display_name_of(&ws, &panes), "scratch");

        panes[0].title = "nvim".into();
        assert_eq!(
            display_name_of(&ws, &panes),
            "scratch",
            "a pane's process title must not rename its workspace"
        );

        ws.tabs[0].sidebar_group = Some("/repo/tty7".into());
        assert_eq!(
            display_name_of(&ws, &panes),
            "tty7",
            "the repo group wins over the raw cwd"
        );

        ws.name = Some("  Release prep  ".into());
        assert_eq!(display_name_of(&ws, &panes), "Release prep");

        assert_eq!(display_name_of(&Workspace::default(), &[]), "Untitled");
    }
}
