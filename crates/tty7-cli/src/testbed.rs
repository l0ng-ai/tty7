use tty7_core::core::machine::{Axis, Machine, PaneNode, PaneRecord, Tab, TabId, Workspace};
use tty7_core::core::session::WorkspaceId;

pub fn two_workspace_machine() -> Machine {
    let api = Workspace {
        id: WorkspaceId::new(),
        name: Some("api".into()),
        last_active: 0,
        tabs: vec![
            Tab {
                id: TabId::new(),
                name: Some("build".into()),
                sidebar_group: None,
                root: PaneNode::Leaf { pane: 1 },
            },
            Tab {
                id: TabId::new(),
                name: None,
                sidebar_group: None,
                root: PaneNode::Split {
                    axis: Axis::Horizontal,
                    ratio: 0.5,
                    a: Box::new(PaneNode::Leaf { pane: 2 }),
                    b: Box::new(PaneNode::Leaf { pane: 3 }),
                },
            },
        ],
        active_tab: None,
        attachment: None,
    };
    let web = Workspace {
        id: WorkspaceId::new(),
        name: Some("web".into()),
        last_active: 0,
        tabs: vec![Tab {
            id: TabId::new(),
            name: None,
            sidebar_group: None,
            root: PaneNode::Leaf { pane: 5 },
        }],
        active_tab: None,
        attachment: None,
    };
    let record = |id: u64, cwd: &str| PaneRecord {
        id,
        cwd: Some(cwd.to_string()),
        title: String::new(),
        ssh_spec: None,
        agent: None,
        live: true,
    };
    Machine {
        workspaces: vec![api, web],
        panes: vec![
            record(1, "C:\\proj"),
            record(2, "C:\\proj"),
            record(3, "C:\\proj\\sub"),
            record(5, "C:\\web"),
        ],
    }
}
