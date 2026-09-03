use std::collections::HashMap;

use gpui::{App, Global};
use tty7_core::core::machine::{LayoutDelta, Machine, PaneRecord, Tab, TabId, Workspace};
use tty7_core::daemon::control::{ControlRequest, ReplyOk};
use tty7_core::host::HostId;

use crate::core::session::WorkspaceId;
use crate::ui::i18n::{L10nKey, t};

/// A write a window made to its own mirror, kept for as long as a pull that
/// was already on its way when it happened has still to land.
type MirrorWrite = Box<dyn Fn(&mut Machine)>;

#[derive(Default)]
pub struct MachineMirrors {
    machines: HashMap<HostId, Machine>,
    pulling: Vec<HostId>,
    /// What this client wrote into a mirror while a pull of it was in flight.
    ///
    /// The tree a `MachineGet` answers is authoritative for everything the
    /// machine knew when it built it — and what it did not know is exactly the
    /// ops a window has just pushed, since a client is left out of the deltas
    /// its own ops raise. The write here is the only copy of those there is,
    /// and a pull dispatched before it and installed after it took that copy
    /// away: the seeded pane records and the pushed tabs went missing again,
    /// by another route (#612, #604). Replayed on top of the tree that lands,
    /// in the order they were made.
    since_pull: HashMap<HostId, Vec<MirrorWrite>>,
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
        if !Self::start_pull(cx, host) {
            return;
        }
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
            cx.update(|cx| Self::finish_pull(cx, host, pulled.map(|machine| *machine)));
        })
        .detach();
    }

    /// Claims the one pull a host is allowed to have on its way, and starts
    /// keeping what this client writes until it lands. `false` if a pull is
    /// already running: its answer is as fresh as this one's would have been.
    fn start_pull(cx: &mut App, host: HostId) -> bool {
        let mirrors = cx.default_global::<Self>();
        if mirrors.pulling.contains(&host) {
            return false;
        }
        mirrors.pulling.push(host);
        mirrors.since_pull.entry(host).or_default();
        true
    }

    /// Installs a pulled tree, and replays over it what this client wrote
    /// while it was on its way.
    ///
    /// The tree wins on everything it speaks about: it is the machine's own
    /// answer, and a mirror that kept its older values over it would be
    /// resurrecting what a pane's death or another window's op had settled.
    /// What it cannot speak about is an op that had not reached the machine
    /// when it was built — the window's own, whose deltas never come back to
    /// it. Those writes go on top: against a tree that turned out to hold
    /// them anyway each one is a no-op, since the seeded records insert only
    /// and the tabs are the tabs the window pushed.
    fn finish_pull(cx: &mut App, host: HostId, pulled: Option<Machine>) {
        let mirrors = cx.default_global::<Self>();
        mirrors.pulling.retain(|h| *h != host);
        let since = mirrors.since_pull.remove(&host).unwrap_or_default();
        let Some(machine) = pulled else {
            return;
        };
        mirrors.machines.insert(host, machine);
        let machine = mirrors.machines.get_mut(&host).expect("just inserted");
        for write in since {
            write(machine);
        }
        cx.refresh_windows();
    }

    /// Writes into a mirror what a window knows about its own machine, and
    /// keeps the write for a pull already on its way (see [`Self::finish_pull`]).
    ///
    /// A mirror that is not here yet is no reason to drop the write: the pull
    /// that will install it is the very one the write has to survive.
    fn write(cx: &mut App, host: HostId, edit: impl Fn(&mut Machine) + 'static) {
        let mirrors = cx.default_global::<Self>();
        if let Some(machine) = mirrors.machines.get_mut(&host) {
            edit(machine);
        }
        if let Some(since) = mirrors.since_pull.get_mut(&host) {
            since.push(Box::new(edit));
        }
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
        Self::write(cx, host, move |machine| {
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
            ws.tabs = tabs.clone();
            ws.active_tab = active;
        });
    }

    /// The projects a window just pushed, recorded for the same reason the
    /// tabs above are: the deltas that carried them are not sent back to the
    /// window that raised them, so this mirror would never hear about them.
    pub fn note_synced_projects(
        cx: &mut App,
        host: HostId,
        machine_ws: WorkspaceId,
        projects: Vec<tty7_core::core::machine::Project>,
    ) {
        Self::write(cx, host, move |machine| {
            if let Some(ws) = machine.workspaces.iter_mut().find(|w| w.id == machine_ws) {
                ws.projects = projects.clone();
            }
        });
    }

    /// Records for the panes this window itself seeded into the machine.
    ///
    /// A window is left out of the deltas its own ops raise, and `TabCreated`
    /// carries no pane record for anyone anyway, so the record the machine
    /// mints for a pane this window created reaches it in nothing it is sent.
    /// `PaneFacts` only close that gap when a fact changes, and a pane spawned
    /// into its cwd and left at the prompt never changes one — the mirror held
    /// tabs full of panes it knew nothing about, the window's own workspace
    /// answered no subject path, an unnamed one read "Untitled" (#612). The
    /// window knows what it seeded, so it records that itself. Insert only: a
    /// record already here came from the machine — a pull, a rider on
    /// `TabRestructured`, `PaneFacts` — and outranks what a seed knows.
    pub fn note_seeded_panes(cx: &mut App, host: HostId, records: Vec<PaneRecord>) {
        Self::write(cx, host, move |machine| {
            for record in &records {
                if !machine.panes.iter().any(|p| p.id == record.id) {
                    machine.panes.push(record.clone());
                }
            }
        });
    }

    /// The name the machine has for a workspace, from a pull that saw it.
    ///
    /// A window is left out of the deltas its own ops raise, so the name it
    /// proposed at creation comes back in nothing it is sent — this is where it
    /// learns the name of the workspace it is showing, and without it the chip
    /// read the directory until some later full pull produced the name and
    /// looked like a rename (#604). `None` is an answer too: a workspace made
    /// by `tty7 new` really has no name, and reads its directory on purpose.
    pub fn note_workspace_name(
        cx: &mut App,
        host: HostId,
        machine_ws: WorkspaceId,
        name: Option<String>,
    ) {
        Self::write(cx, host, move |machine| {
            if let Some(ws) = machine.workspaces.iter_mut().find(|w| w.id == machine_ws) {
                ws.name = name.clone();
            }
        });
        cx.refresh_windows();
    }

    pub fn note_workspace_op(cx: &mut App, host: HostId, request: &ControlRequest) {
        let request = request.clone();
        // Read now rather than inside the write: a replay of the touch is the
        // same touch, and it should not creep forward to the moment a pull
        // happened to land.
        let touched = crate::ui::home::now_secs();
        Self::write(cx, host, move |machine| match &request {
            ControlRequest::WorkspaceRename { workspace, name } => {
                if let Some(ws) = machine.workspaces.iter_mut().find(|w| w.id == *workspace) {
                    ws.name = name.clone();
                }
            }
            ControlRequest::WorkspaceTouch { workspace } => {
                if let Some(ws) = machine.workspaces.iter_mut().find(|w| w.id == *workspace) {
                    ws.last_active = touched;
                }
            }
            ControlRequest::WorkspaceRemove { workspace } => {
                machine.workspaces.retain(|w| w.id != *workspace);
            }
            _ => {}
        });
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
        LayoutDelta::TabProjectChanged { tab, project } => {
            let Some(t) = ws.tabs.iter_mut().find(|t| t.id == *tab) else {
                return false;
            };
            t.project = *project;
            true
        }
        LayoutDelta::ProjectCreated { at, project } => {
            ws.projects.retain(|p| p.id != project.id);
            let at = (*at).min(ws.projects.len());
            ws.projects.insert(at, project.clone());
            true
        }
        LayoutDelta::ProjectRenamed { project, name } => {
            let Some(p) = ws.projects.iter_mut().find(|p| p.id == *project) else {
                return false;
            };
            p.name = name.clone();
            true
        }
        LayoutDelta::ProjectRerooted { project, root } => {
            let Some(p) = ws.projects.iter_mut().find(|p| p.id == *project) else {
                return false;
            };
            p.root = root.clone();
            true
        }
        LayoutDelta::ProjectMoved { project, to } => {
            let Some(from) = ws.projects.iter().position(|p| p.id == *project) else {
                return false;
            };
            let moved = ws.projects.remove(from);
            ws.projects.insert((*to).min(ws.projects.len()), moved);
            true
        }
        LayoutDelta::ProjectDeleted { project } => {
            let before = ws.projects.len();
            ws.projects.retain(|p| p.id != *project);
            for tab in &mut ws.tabs {
                if tab.project == Some(*project) {
                    tab.project = None;
                }
            }
            ws.projects.len() != before
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
    match view_of(cx, entry) {
        Some((ws, panes)) => Some(display_name_of(ws, panes)),
        None => entry.label.clone(),
    }
}

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
    let (ws, panes) = view_of(cx, entry)?;
    Some((display_name_of(ws, panes), subject_path_of(ws, panes)))
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
    use tty7_core::core::machine::{Axis, PaneNode, PaneSeed, Tab, TabId};

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

    /// #604: a window is left out of the deltas its own ops raise, so the name
    /// the machine gave the workspace it just created reaches it only through
    /// the answer to that create. Once it has, the full tree a rebuild pulls
    /// says the same thing and nothing on screen moves; until it did, the chip
    /// read the directory and the first full pull looked like a rename.
    #[gpui::test]
    fn a_named_workspace_reads_the_same_before_and_after_a_full_pull(
        cx: &mut gpui::TestAppContext,
    ) {
        use crate::core::session::{WindowView, WindowViews, WorkspaceStore};

        cx.update(|cx| {
            let entry = WindowView::default();
            let id = entry.id;
            WorkspaceStore::install_for_test(
                cx,
                WindowViews {
                    views: vec![entry.clone()],
                    active: None,
                },
            );

            // What the window knows the moment its create is answered: a
            // workspace of its own, and the name the machine put on it.
            MachineMirrors::install(cx, HostId::LOCAL, Machine::default());
            MachineMirrors::note_synced_workspace(cx, HostId::LOCAL, id, vec![leaf_tab(1)], None);
            MachineMirrors::note_workspace_name(
                cx,
                HostId::LOCAL,
                id,
                Some("keen-marten".to_string()),
            );
            assert_eq!(display_name(cx, &entry).as_deref(), Some("keen-marten"));

            // The rebuild, pulling the whole tree: the same answer.
            let pulled = |name: Option<&str>| Machine {
                workspaces: vec![Workspace {
                    id,
                    name: name.map(str::to_string),
                    tabs: vec![leaf_tab(1)],
                    ..Workspace::default()
                }],
                panes: vec![PaneRecord {
                    cwd: Some("/work/verify-main".into()),
                    ..PaneRecord::new(1)
                }],
            };
            MachineMirrors::install(cx, HostId::LOCAL, pulled(Some("keen-marten")));
            assert_eq!(
                display_name(cx, &entry).as_deref(),
                Some("keen-marten"),
                "the rebuild has nothing new to say, so nothing renames itself"
            );

            // A name the user chose outranks the one they were given.
            MachineMirrors::install(cx, HostId::LOCAL, pulled(Some("deploy")));
            assert_eq!(display_name(cx, &entry).as_deref(), Some("deploy"));

            // And "the machine has no name for it" is an answer too — that is
            // what `tty7 new` leaves behind, and it reads as its directory.
            MachineMirrors::install(cx, HostId::LOCAL, pulled(None));
            assert_eq!(display_name(cx, &entry).as_deref(), Some("verify-main"));
            MachineMirrors::note_workspace_name(cx, HostId::LOCAL, id, None);
            assert_eq!(display_name(cx, &entry).as_deref(), Some("verify-main"));
        });
    }

    /// #612: the deltas that carry a pane's record — `TabRestructured`'s
    /// rider, `PaneFacts` on a change; `TabCreated` carries none at all —
    /// reach every window but the one whose op raised them. The window seeded
    /// the pane, so it records what it seeded; the full tree a rebuild pulls
    /// then says the same thing and nothing on screen moves. Until it did,
    /// the window's own workspace answered no subject path: an unnamed one
    /// read "Untitled", and saving geometry stamped a null subject over what
    /// views.json remembered.
    #[gpui::test]
    fn a_seeded_pane_reads_the_same_before_and_after_a_full_pull(cx: &mut gpui::TestAppContext) {
        use crate::core::session::{WindowView, WindowViews, WorkspaceStore};

        cx.update(|cx| {
            let entry = WindowView::default();
            let id = entry.id;
            WorkspaceStore::install_for_test(
                cx,
                WindowViews {
                    views: vec![entry.clone()],
                    active: None,
                },
            );

            // What the window knows the moment it pushes its first tab: the
            // tab it made, and the seed it sent along.
            MachineMirrors::install(cx, HostId::LOCAL, Machine::default());
            MachineMirrors::note_synced_workspace(cx, HostId::LOCAL, id, vec![leaf_tab(1)], None);
            let seeded = PaneSeed {
                cwd: Some("/work/verify-main".into()),
                ..PaneSeed::bare(1)
            }
            .into_record(true);
            MachineMirrors::note_seeded_panes(cx, HostId::LOCAL, vec![seeded]);
            assert_eq!(display_name(cx, &entry).as_deref(), Some("verify-main"));
            assert_eq!(
                subject_path(cx, &entry).as_deref(),
                Some("/work/verify-main")
            );

            // The rebuild, pulling the whole tree: the same answer.
            MachineMirrors::install(
                cx,
                HostId::LOCAL,
                Machine {
                    workspaces: vec![Workspace {
                        id,
                        tabs: vec![leaf_tab(1)],
                        ..Workspace::default()
                    }],
                    // The record the machine minted for the same seed, written
                    // out rather than the copy above reused: a pull that hands
                    // back the very value the window put in is only agreeing
                    // with itself.
                    panes: vec![PaneRecord {
                        cwd: Some("/work/verify-main".into()),
                        live: true,
                        ..PaneRecord::new(1)
                    }],
                },
            );
            assert_eq!(display_name(cx, &entry).as_deref(), Some("verify-main"));
            assert_eq!(
                subject_path(cx, &entry).as_deref(),
                Some("/work/verify-main")
            );
        });
    }

    #[gpui::test]
    fn a_seed_fills_only_the_records_the_mirror_lacks(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            // Facts the machine broadcast before the sync write got here: a
            // reported title, a deeper cwd. The seed's flat idea of the pane
            // must not roll them back.
            let facts = PaneRecord {
                cwd: Some("/work/deeper".into()),
                osc_title: Some("me@host:~/work/deeper".into()),
                live: true,
                ..PaneRecord::new(1)
            };
            MachineMirrors::install(
                cx,
                HostId::LOCAL,
                Machine {
                    workspaces: Vec::new(),
                    panes: vec![facts.clone()],
                },
            );
            let known = PaneSeed {
                cwd: Some("/work".into()),
                ..PaneSeed::bare(1)
            }
            .into_record(false);
            let fresh = PaneSeed {
                cwd: Some("/work/api".into()),
                ..PaneSeed::bare(2)
            }
            .into_record(true);
            MachineMirrors::note_seeded_panes(cx, HostId::LOCAL, vec![known, fresh]);
            let machine = MachineMirrors::machine(cx, HostId::LOCAL).expect("installed");
            assert_eq!(machine.panes.len(), 2);
            assert_eq!(
                machine.panes[0], facts,
                "what the machine said outranks the seed"
            );
            assert_eq!(machine.panes[1].cwd.as_deref(), Some("/work/api"));
            assert!(machine.panes[1].live);
        });
    }

    /// #612 by its other route: a `MachineGet` already on its way when the
    /// window wrote down what it had just pushed was built before that push,
    /// and landing it whole put the mirror back to a tree that has never heard
    /// of the tab or the pane — the same workspace answering no subject path,
    /// with no later op to write it down again. What the window pushed goes
    /// back on top of the tree that lands; what that tree does say still
    /// outranks it; and once it has landed the writes are spent.
    #[gpui::test]
    fn a_pull_already_on_its_way_lands_under_what_the_window_pushed(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            // Nothing pulled yet, which is where a window that opens with its
            // machine's link coming up starts: the pull below is the one that
            // will install this mirror at all.
            let id = WorkspaceId::new();
            assert!(MachineMirrors::start_pull(cx, HostId::LOCAL));

            // What the window pushes while that pull is on its way: a tab of
            // its own, and the seeds behind the panes in it.
            let pushed = leaf_tab(5);
            MachineMirrors::note_synced_workspace(
                cx,
                HostId::LOCAL,
                id,
                vec![pushed.clone()],
                None,
            );
            let seeded = |pane, cwd: &str| {
                PaneSeed {
                    cwd: Some(cwd.into()),
                    ..PaneSeed::bare(pane)
                }
                .into_record(true)
            };
            MachineMirrors::note_seeded_panes(
                cx,
                HostId::LOCAL,
                vec![seeded(5, "/work/verify-main"), seeded(6, "/work")],
            );

            // The tree the machine built before any of that: an older tab, and
            // a pane 6 it has since watched move on. Pane 5 it has never seen.
            MachineMirrors::finish_pull(
                cx,
                HostId::LOCAL,
                Some(Machine {
                    workspaces: vec![Workspace {
                        id,
                        tabs: vec![leaf_tab(4)],
                        ..Workspace::default()
                    }],
                    panes: vec![PaneRecord {
                        cwd: Some("/work/deeper".into()),
                        osc_title: Some("me@host:~/work/deeper".into()),
                        live: true,
                        ..PaneRecord::new(6)
                    }],
                }),
            );

            let machine = MachineMirrors::machine(cx, HostId::LOCAL).expect("the pull installed");
            assert_eq!(
                machine.workspaces[0].tabs,
                vec![pushed],
                "the tabs the window pushed are not rolled back by a tree older than them"
            );
            let cwd_of = |pane: u64| {
                machine
                    .panes
                    .iter()
                    .find(|p| p.id == pane)
                    .and_then(|p| p.cwd.clone())
            };
            assert_eq!(
                cwd_of(5).as_deref(),
                Some("/work/verify-main"),
                "and the record for the pane it seeded is still the only one there is"
            );
            assert_eq!(
                cwd_of(6).as_deref(),
                Some("/work/deeper"),
                "while a pane the machine did speak about keeps what it said"
            );

            // Spent: the next pull is the machine's own word, with nothing of
            // the window's replayed under it a second time.
            assert!(MachineMirrors::start_pull(cx, HostId::LOCAL));
            MachineMirrors::finish_pull(cx, HostId::LOCAL, Some(Machine::default()));
            let machine = MachineMirrors::machine(cx, HostId::LOCAL).expect("installed");
            assert!(
                machine.workspaces.is_empty() && machine.panes.is_empty(),
                "a tree that has dropped them says so: {machine:?}"
            );
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
            project: None,
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
