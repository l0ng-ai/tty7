use std::collections::HashMap;

use gpui::{App, Global};
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
/// own, so the switcher's tab column reads them from here.
#[derive(Debug, Clone, PartialEq)]
pub struct TabView {
    pub id: TabId,
    pub name: Option<String>,
    pub title: String,
    pub cwd: Option<String>,
    pub agent: Option<crate::core::cli_agent::CLIAgent>,
    pub status: Option<crate::core::cli_agent::AgentStatus>,
    pub live: bool,
    pub panes: usize,
}

pub fn tab_views_for(cx: &App, client_ws: WorkspaceId) -> Option<(Vec<TabView>, Option<TabId>)> {
    let entry = crate::core::session::WorkspaceStore::all(cx).get(client_ws)?;
    let (ws, panes) = view_of(cx, entry)?;
    Some((tab_views_of(ws, panes), ws.active_tab))
}

pub fn tab_views_of(ws: &Workspace, panes: &[PaneRecord]) -> Vec<TabView> {
    ws.tabs
        .iter()
        .map(|tab| {
            let ids = tab.root.pane_ids();
            let records: Vec<&PaneRecord> = ids
                .iter()
                .filter_map(|id| panes.iter().find(|p| p.id == *id))
                .collect();
            // The first pane stands in for the tab, the same way the strip shows
            // its focused leaf — but any pane running an agent wins, since that
            // is what someone scanning the list is looking for.
            let head = records.first();
            let facts = records.iter().find_map(|p| p.agent.as_ref());
            TabView {
                id: tab.id,
                name: tab.name.clone(),
                title: head.map(|p| p.title.clone()).unwrap_or_default(),
                cwd: head.and_then(|p| p.cwd.clone()),
                agent: facts.map(|f| f.agent),
                status: facts.and_then(|f| f.status),
                live: records.iter().any(|p| p.live),
                panes: ids.len(),
            }
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
