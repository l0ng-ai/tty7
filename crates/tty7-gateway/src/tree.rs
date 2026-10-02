//! The machine tree, as the phone sees it.
//!
//! The daemon's tree is the document it persists — split ratios, pinned
//! groups, SSH specs, who holds which workspace. The phone needs a list to tap
//! through: workspaces, their tabs, the panes in each, and which of those have
//! an agent waiting on someone. This module is that reduction, and the naming
//! goes through `tab_view` so a tab reads the same here as in the CLI and the
//! desktop's switcher.

use std::collections::HashSet;
use std::path::PathBuf;

use tty7_core::core::cli_agent::{AgentSessionState, AgentStatus as CoreStatus, CLIAgent};
use tty7_core::core::group_key::{AutoKey, GroupKey, auto_names, pinned_names, place};
use tty7_core::core::machine::{Machine, PaneRecord, Workspace};
use tty7_core::core::tab_view::{TabLabel, strip_host_prefix, strip_status_mark, tab_views_of};
use tty7_core::daemon::control::PaneAgentState;
use tty7_mobile_proto::{
    AgentStatus, AgentView, GroupView, PaneView, TabView, Tree, WorkspaceView,
};

pub fn build(host: &str, machine: &Machine, agents: &[PaneAgentState]) -> Tree {
    build_with(host, machine, agents, |_| None, None)
}

/// [`build`], placing a tab the desktop has not filed yet by the repository
/// `repo_of` finds its directory in. The desktop files a tab as its sidebar
/// draws it, so one opened from the phone while that window is out of sight
/// would sit in Ungrouped until someone looked.
pub fn build_with(
    host: &str,
    machine: &Machine,
    agents: &[PaneAgentState],
    repo_of: impl Fn(&str) -> Option<PathBuf>,
    running: Option<&HashSet<u64>>,
) -> Tree {
    let mut workspaces: Vec<_> = machine.workspaces.iter().collect();
    // Most recently used first: on a phone the one you were just in is the
    // one you are coming back to.
    workspaces.sort_by_key(|ws| std::cmp::Reverse(ws.last_active));
    Tree {
        host: host.to_string(),
        workspaces: workspaces
            .into_iter()
            .map(|ws| {
                let views = tab_views_of(ws, &machine.panes);
                WorkspaceView {
                    id: ws.id.to_string(),
                    name: ws
                        .name
                        .clone()
                        .filter(|n| !n.trim().is_empty())
                        .unwrap_or_else(|| "-".to_string()),
                    tabs: ws
                        .tabs
                        .iter()
                        .zip(views)
                        .map(|(tab, view)| TabView {
                            id: tab.id.to_string(),
                            name: tab_name(view.label()),
                            hibernated: tab.hibernated,
                            panes: tab
                                .root
                                .pane_ids()
                                .into_iter()
                                .map(|id| {
                                    let mut view = pane_view(id, &machine.panes, agents);
                                    // An asleep tab's panes are stopped on purpose,
                                    // and say so as the tab.
                                    view.stopped = !tab.hibernated
                                        && running.is_some_and(|running| !running.contains(&id));
                                    view
                                })
                                .collect(),
                        })
                        .collect(),
                    groups: groups_of(ws, &machine.panes, &repo_of),
                    active_tab: ws.active_tab.map(|t| t.to_string()),
                }
            })
            .collect(),
        remotes: Vec::new(),
    }
}

/// The workspace's sidebar groups, drawn as the desktop draws them: pinned
/// groups in their order, then a group per repository or SSH host in the
/// order their first tab comes, then the tabs in neither.
///
/// A tab's auto group is the one the desktop last filed it under
/// (`last_auto`): the desktop works it out by probing the tab's directory, and
/// writes the answer down so other readers need not. Auto grouping is taken
/// to be on, as it is by default; a desktop that turned it off still keeps
/// these hints, so its phone shows the repo groups its sidebar does not.
fn groups_of(
    ws: &Workspace,
    panes: &[PaneRecord],
    repo_of: &dyn Fn(&str) -> Option<PathBuf>,
) -> Vec<GroupView> {
    let keys: Vec<Option<GroupKey>> = ws
        .tabs
        .iter()
        .map(|tab| {
            let auto = tab.last_auto.clone().or_else(|| {
                let first = *tab.root.pane_ids().first()?;
                let cwd = panes.iter().find(|p| p.id == first)?.cwd.as_deref()?;
                repo_of(cwd).map(AutoKey::Repo)
            });
            place(tab.group, &ws.groups, true, auto)
        })
        .collect();
    let members = |key: Option<&GroupKey>| -> Vec<String> {
        ws.tabs
            .iter()
            .zip(&keys)
            .filter(|(_, k)| k.as_ref() == key)
            .map(|(tab, _)| tab.id.to_string())
            .collect()
    };
    let mut auto_order: Vec<&AutoKey> = Vec::new();
    for key in keys.iter().flatten().filter_map(GroupKey::auto) {
        if !auto_order.contains(&key) {
            auto_order.push(key);
        }
    }

    let mut groups: Vec<GroupView> = ws
        .groups
        .pinned
        .iter()
        .zip(pinned_names(&ws.groups.pinned))
        .map(|(group, name)| GroupView {
            name: Some(name),
            pinned: true,
            collapsed: group.collapsed,
            tabs: members(Some(&GroupKey::Pinned(group.id))),
        })
        .collect();
    groups.extend(
        auto_order
            .iter()
            .zip(auto_names(&auto_order))
            .map(|(key, name)| GroupView {
                name: Some(name),
                pinned: false,
                collapsed: ws.groups.auto_collapsed.contains(key),
                tabs: members(Some(&GroupKey::Auto((*key).clone()))),
            }),
    );
    let rest = members(None);
    if !rest.is_empty() {
        // Named only beside auto groups, as the desktop names it.
        let named = !auto_order.is_empty();
        groups.push(GroupView {
            name: named.then(|| "Ungrouped".to_string()),
            pinned: false,
            collapsed: named && ws.groups.ungrouped_collapsed,
            tabs: rest,
        });
    }
    groups
}

fn tab_name(label: TabLabel<'_>) -> String {
    match label {
        TabLabel::Named(name) => name.to_string(),
        TabLabel::Osc(title) => strip_host_prefix(title).to_string(),
        TabLabel::Agent(agent) => agent.display_name().to_string(),
        TabLabel::Cwd(cwd) => path_leaf(cwd).to_string(),
        TabLabel::Process(title) => title.to_string(),
        TabLabel::Unknown => "-".to_string(),
    }
}

fn pane_view(id: u64, panes: &[PaneRecord], agents: &[PaneAgentState]) -> PaneView {
    let record = panes.iter().find(|p| p.id == id);
    let title = record
        .and_then(|p| p.osc_title.as_deref())
        .map(|t| strip_host_prefix(strip_status_mark(t.trim())))
        .filter(|t| !t.is_empty())
        .or_else(|| record.map(|p| p.title.as_str()).filter(|t| !t.is_empty()))
        .unwrap_or("-")
        .to_string();
    // The live state wins: the record's status is what was last persisted,
    // while `AgentStates` is what the daemon's hook listener heard just now.
    let live = agents.iter().find(|a| a.pane_id == id);
    let agent = match live {
        Some(live) => live
            .agent
            .or_else(|| record.and_then(|p| p.agent.as_ref()).map(|f| f.agent))
            .map(|agent| agent_view(agent, &live.state)),
        None => record
            .and_then(|p| p.agent.as_ref())
            .map(|facts| AgentView {
                kind: facts.agent.slug().to_string(),
                status: status(facts.status.unwrap_or_default()),
                message: None,
            }),
    };
    PaneView {
        id,
        title,
        cwd: record.and_then(|p| p.cwd.clone()),
        agent,
        stopped: false,
    }
}

pub fn agent_view(agent: CLIAgent, state: &AgentSessionState) -> AgentView {
    AgentView {
        kind: agent.slug().to_string(),
        status: status(state.status),
        message: state.message.clone(),
    }
}

pub fn status(s: CoreStatus) -> AgentStatus {
    match s {
        CoreStatus::Idle => AgentStatus::Idle,
        CoreStatus::Working => AgentStatus::Working,
        CoreStatus::Waiting => AgentStatus::Waiting,
        CoreStatus::Done => AgentStatus::Done,
    }
}

fn path_leaf(path: &str) -> &str {
    let trimmed = path.trim_end_matches(['/', '\\']);
    trimmed
        .rsplit(['/', '\\'])
        .next()
        .filter(|leaf| !leaf.is_empty())
        .unwrap_or(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tty7_core::core::machine::{AgentFacts, Tab, Workspace};

    fn record(id: u64, title: &str, osc: Option<&str>, cwd: &str) -> PaneRecord {
        let mut value = serde_json::json!({
            "id": id,
            "title": title,
            "cwd": cwd,
        });
        if let Some(osc) = osc {
            value["osc_title"] = osc.into();
        }
        serde_json::from_value(value).expect("a pane record from its minimal fields")
    }

    fn workspace(name: &str, last_active: u64, tabs: Vec<Tab>) -> Workspace {
        Workspace {
            name: Some(name.to_string()),
            last_active,
            tabs,
            ..Workspace::default()
        }
    }

    #[test]
    fn workspaces_come_most_recent_first_with_their_tabs_and_panes() {
        let mut named = Tab::leaf(1);
        named.name = Some("build".into());
        let machine = Machine {
            workspaces: vec![
                workspace("old-owl", 10, vec![named]),
                workspace("pale-otter", 20, vec![Tab::leaf(2)]),
            ],
            panes: vec![
                record(1, "zsh", None, "/src/a"),
                record(2, "zsh", Some("me@box:~/src/b"), "/src/b"),
            ],
        };
        let tree = build("studio", &machine, &[]);
        assert_eq!(tree.host, "studio");
        let names: Vec<_> = tree.workspaces.iter().map(|w| w.name.as_str()).collect();
        assert_eq!(names, ["pale-otter", "old-owl"]);
        assert_eq!(tree.workspaces[0].tabs[0].name, "~/src/b");
        assert_eq!(tree.workspaces[0].tabs[0].panes[0].title, "~/src/b");
        assert_eq!(tree.workspaces[1].tabs[0].name, "build");
        assert_eq!(
            tree.workspaces[1].tabs[0].panes[0].cwd.as_deref(),
            Some("/src/a")
        );
    }

    #[test]
    fn live_agent_state_beats_the_persisted_one() {
        let mut rec = record(3, "node", None, "/src");
        rec.agent = Some(AgentFacts {
            agent: CLIAgent::Claude,
            session_id: None,
            launch_argv: None,
            status: Some(CoreStatus::Working),
        });
        let machine = Machine {
            workspaces: vec![workspace("w", 1, vec![Tab::leaf(3), Tab::leaf(4)])],
            panes: vec![rec, record(4, "zsh", None, "/src")],
        };

        let persisted = build("h", &machine, &[]);
        let pane = &persisted.workspaces[0].tabs[0].panes[0];
        let agent = pane.agent.as_ref().unwrap();
        assert_eq!(
            (agent.kind.as_str(), agent.status),
            ("claude", AgentStatus::Working)
        );

        let live = [PaneAgentState {
            pane_id: 3,
            agent: None,
            state: AgentSessionState {
                status: CoreStatus::Waiting,
                message: Some("needs permission".into()),
                ..AgentSessionState::default()
            },
        }];
        let tree = build("h", &machine, &live);
        let agent = tree.workspaces[0].tabs[0].panes[0].agent.clone().unwrap();
        assert_eq!(agent.status, AgentStatus::Waiting);
        assert_eq!(agent.message.as_deref(), Some("needs permission"));
        assert!(tree.workspaces[0].tabs[1].panes[0].agent.is_none());
    }

    #[test]
    fn tabs_fall_into_the_desktops_groups_in_its_order() {
        use tty7_core::core::group_key::PinnedGroup;
        let mut ws = workspace("w", 1, (1..=5).map(Tab::leaf).collect());
        let mut urgent = PinnedGroup::label("urgent");
        urgent.collapsed = true;
        let urgent_id = urgent.id;
        ws.groups.pinned.push(urgent);
        // A pinned group nobody is in still has its place.
        ws.groups.pinned.push(PinnedGroup::label("later"));
        ws.groups
            .auto_collapsed
            .push(AutoKey::Repo("/w/api".into()));
        ws.tabs[0].last_auto = Some(AutoKey::Repo("/w/tty7".into()));
        ws.tabs[1].group = Some(urgent_id);
        // A pin outranks the repo the tab is in.
        ws.tabs[1].last_auto = Some(AutoKey::Repo("/w/tty7".into()));
        ws.tabs[2].last_auto = Some(AutoKey::Repo("/w/api".into()));
        ws.tabs[3].last_auto = Some(AutoKey::Repo("/w/tty7".into()));
        ws.active_tab = Some(ws.tabs[3].id);
        let ids: Vec<String> = ws.tabs.iter().map(|t| t.id.to_string()).collect();
        let machine = Machine {
            workspaces: vec![ws],
            panes: Vec::new(),
        };

        let view = &build("h", &machine, &[]).workspaces[0];
        let groups: Vec<_> = view
            .groups
            .iter()
            .map(|g| (g.name.as_deref(), g.pinned, g.collapsed, g.tabs.clone()))
            .collect();
        assert_eq!(
            groups,
            [
                (Some("urgent"), true, true, vec![ids[1].clone()]),
                (Some("later"), true, false, vec![]),
                (
                    Some("tty7"),
                    false,
                    false,
                    vec![ids[0].clone(), ids[3].clone()]
                ),
                (Some("api"), false, true, vec![ids[2].clone()]),
                (Some("Ungrouped"), false, false, vec![ids[4].clone()]),
            ]
        );
        assert_eq!(view.active_tab.as_deref(), Some(ids[3].as_str()));
    }

    #[test]
    fn a_tab_the_desktop_has_not_filed_goes_by_its_directorys_repo() {
        let mut ws = workspace("w", 1, vec![Tab::leaf(1), Tab::leaf(2), Tab::leaf(3)]);
        ws.tabs[0].last_auto = Some(AutoKey::Repo("/w/k6".into()));
        let ids: Vec<String> = ws.tabs.iter().map(|t| t.id.to_string()).collect();
        let machine = Machine {
            workspaces: vec![ws],
            panes: vec![
                record(1, "zsh", None, "/w/k6"),
                record(2, "zsh", None, "/w/k6/src"),
                record(3, "zsh", None, "/tmp"),
            ],
        };
        let repo_of = |cwd: &str| cwd.starts_with("/w/k6").then(|| PathBuf::from("/w/k6"));
        let view = &build_with("h", &machine, &[], repo_of, None).workspaces[0];
        let groups: Vec<_> = view
            .groups
            .iter()
            .map(|g| (g.name.as_deref(), g.tabs.clone()))
            .collect();
        assert_eq!(
            groups,
            [
                (Some("k6"), vec![ids[0].clone(), ids[1].clone()]),
                (Some("Ungrouped"), vec![ids[2].clone()]),
            ]
        );
    }

    #[test]
    fn a_pane_the_server_is_not_running_is_marked_stopped() {
        let mut asleep = Tab::leaf(3);
        asleep.hibernated = true;
        let machine = Machine {
            workspaces: vec![workspace("w", 1, vec![Tab::leaf(1), Tab::leaf(2), asleep])],
            panes: Vec::new(),
        };
        let running = HashSet::from([1]);
        let view = &build_with("h", &machine, &[], |_| None, Some(&running)).workspaces[0];
        let stopped: Vec<bool> = view.tabs.iter().map(|t| t.panes[0].stopped).collect();
        // The asleep tab says so as itself, not as a pane that stopped.
        assert_eq!(stopped, [false, true, false]);
        // A server that could not be asked marks nothing.
        let view = &build("h", &machine, &[]).workspaces[0];
        assert!(view.tabs.iter().all(|t| !t.panes[0].stopped));
    }

    #[test]
    fn a_workspace_without_groups_is_one_headless_list() {
        let machine = Machine {
            workspaces: vec![workspace("w", 1, vec![Tab::leaf(1), Tab::leaf(2)])],
            panes: Vec::new(),
        };
        let view = &build("h", &machine, &[]).workspaces[0];
        assert_eq!(view.groups.len(), 1);
        assert_eq!(view.groups[0].name, None);
        assert_eq!(view.groups[0].tabs.len(), 2);
    }

    #[test]
    fn a_path_is_cut_to_its_leaf() {
        assert_eq!(path_leaf("/a/b/c/"), "c");
        assert_eq!(path_leaf(r"C:\proj\x"), "x");
        assert_eq!(path_leaf("/"), "/");
    }
}
