//! What a tab looks like to someone who is not the window showing it.
//!
//! A window renders its own tabs from live terminals: OSC titles, agent
//! chatter, unread counts. Everyone else — the switcher listing a workspace
//! it does not own, `tty7 tab ls` on the other side of a socket — has only
//! the machine tree. This is the reading of that tree, kept in one place so
//! the CLI and the GUI name a tab the same way.

use crate::core::cli_agent::{AgentStatus, CLIAgent};
use crate::core::machine::{PaneRecord, TabId, Workspace};

/// Deliberately not serialisable: it is a reading of the machine tree, and
/// both sides that want one have the tree already. Putting it on the wire
/// would be sending a conclusion where the evidence has already gone.
#[derive(Debug, Clone, PartialEq)]
pub struct TabView {
    pub id: TabId,
    pub name: Option<String>,
    /// The foreground process of the tab's leading pane — "zsh", "vim". Not
    /// the OSC title: the tree never sees one.
    pub title: String,
    pub cwd: Option<String>,
    pub agent: Option<CLIAgent>,
    pub status: Option<AgentStatus>,
    pub live: bool,
    pub panes: usize,
}

/// Where a tab's displayed name comes from, best evidence first. Callers
/// render it themselves: a path is abbreviated one way in a 20-column tab
/// strip and another way in a terminal table, and only the GUI has a
/// translated string for a tab with nothing to say.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TabLabel<'a> {
    /// Someone named this tab, so nothing else gets a say.
    Named(&'a str),
    /// No name, but an agent is running in it — which is what anyone
    /// scanning a list of tabs is looking for.
    Agent(CLIAgent),
    /// The working directory of the tab's leading pane.
    Cwd(&'a str),
    /// The foreground process name. Thin, but it beats nothing.
    Process(&'a str),
    /// A tab holding a pane the tree knows nothing about.
    Unknown,
}

impl TabView {
    pub fn label(&self) -> TabLabel<'_> {
        if let Some(name) = self
            .name
            .as_deref()
            .map(str::trim)
            .filter(|n| !n.is_empty())
        {
            return TabLabel::Named(name);
        }
        if let Some(agent) = self.agent {
            return TabLabel::Agent(agent);
        }
        if let Some(cwd) = self.cwd.as_deref().map(str::trim).filter(|c| !c.is_empty()) {
            return TabLabel::Cwd(cwd);
        }
        match self.title.trim() {
            "" => TabLabel::Unknown,
            title => TabLabel::Process(title),
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::machine::{AgentFacts, Tab};

    fn view() -> TabView {
        TabView {
            id: TabId::new(),
            name: None,
            title: String::new(),
            cwd: None,
            agent: None,
            status: None,
            live: true,
            panes: 1,
        }
    }

    #[test]
    fn a_label_prefers_the_name_then_the_agent_then_the_place() {
        let named = TabView {
            name: Some("  deploy  ".into()),
            agent: Some(CLIAgent::Claude),
            cwd: Some("/work".into()),
            ..view()
        };
        assert_eq!(named.label(), TabLabel::Named("deploy"));

        let working = TabView {
            agent: Some(CLIAgent::Claude),
            cwd: Some("/work".into()),
            ..view()
        };
        assert_eq!(working.label(), TabLabel::Agent(CLIAgent::Claude));

        let plain = TabView {
            cwd: Some("/work".into()),
            title: "zsh".into(),
            ..view()
        };
        assert_eq!(plain.label(), TabLabel::Cwd("/work"));
    }

    #[test]
    fn a_blank_name_is_no_name_and_a_bare_shell_falls_back_to_its_process() {
        let blank = TabView {
            name: Some("   ".into()),
            title: "zsh".into(),
            ..view()
        };
        assert_eq!(blank.label(), TabLabel::Process("zsh"));
        assert_eq!(view().label(), TabLabel::Unknown);
    }

    #[test]
    fn a_tab_is_read_through_its_leading_pane_but_any_agent_in_it_wins() {
        let mut ws = Workspace::default();
        let mut tab = Tab::leaf(1);
        tab.root = crate::core::machine::PaneNode::Split {
            axis: crate::core::machine::Axis::Horizontal,
            ratio: 0.5,
            a: Box::new(crate::core::machine::PaneNode::Leaf { pane: 1 }),
            b: Box::new(crate::core::machine::PaneNode::Leaf { pane: 2 }),
        };
        ws.tabs.push(tab);

        let panes = vec![
            PaneRecord {
                cwd: Some("/work".into()),
                title: "zsh".into(),
                live: true,
                ..PaneRecord::new(1)
            },
            PaneRecord {
                agent: Some(AgentFacts {
                    agent: CLIAgent::Claude,
                    session_id: None,
                    launch_argv: None,
                    status: None,
                }),
                ..PaneRecord::new(2)
            },
        ];

        let views = tab_views_of(&ws, &panes);
        assert_eq!(views.len(), 1);
        assert_eq!(views[0].cwd.as_deref(), Some("/work"));
        assert_eq!(views[0].agent, Some(CLIAgent::Claude));
        assert_eq!(views[0].panes, 2);
        assert!(views[0].live, "one live pane makes the tab live");
    }
}
