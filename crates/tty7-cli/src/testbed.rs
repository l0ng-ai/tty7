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
                group: None,
                last_auto: None,
                root: PaneNode::Leaf { pane: 1 },
                hibernated: false,
            },
            Tab {
                id: TabId::new(),
                name: None,
                group: None,
                last_auto: None,
                root: PaneNode::Split {
                    axis: Axis::Horizontal,
                    ratio: 0.5,
                    a: Box::new(PaneNode::Leaf { pane: 2 }),
                    b: Box::new(PaneNode::Leaf { pane: 3 }),
                },
                hibernated: false,
            },
        ],
        active_tab: None,
        attachment: None,
        groups: Default::default(),
    };
    let web = Workspace {
        id: WorkspaceId::new(),
        name: Some("web".into()),
        last_active: 0,
        tabs: vec![Tab {
            id: TabId::new(),
            name: None,
            group: None,
            last_auto: None,
            root: PaneNode::Leaf { pane: 5 },
            hibernated: false,
        }],
        active_tab: None,
        attachment: None,
        groups: Default::default(),
    };
    let record = |id: u64, cwd: &str| PaneRecord {
        id,
        cwd: Some(cwd.to_string()),
        title: String::new(),
        osc_title: None,
        ssh_spec: None,
        agent: None,
        shell: None,
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
