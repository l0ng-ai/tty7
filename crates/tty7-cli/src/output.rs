use unicode_width::UnicodeWidthStr;

use tty7_core::core::machine::{Machine, PaneNode, Workspace};
use tty7_core::core::session::WorkspaceId;
use tty7_core::core::tab_view::{TabLabel, TabView, tab_views_of};
use tty7_core::daemon::control::{PaneAgentState, RouteInfo, ServerStatus};
use tty7_core::daemon::protocol::{PaneInfo, PaneProcs};

use crate::resolve;

/// What to call a tab in a table or a tree. Almost no tab carries a name —
/// the GUI's strip reads OSC titles the machine tree never sees — so a column
/// printing `tab.name` alone comes out empty for a window full of work. The
/// evidence ranking is shared with the GUI; only the rendering is ours.
pub fn tab_label(view: &TabView) -> String {
    match view.label() {
        TabLabel::Named(name) => name.to_string(),
        TabLabel::Agent(agent) => agent.display_name().to_string(),
        // The tree prints every pane's full cwd right underneath, and a table
        // has no room for one anyway: the leaf is what tells tabs apart.
        TabLabel::Cwd(cwd) => path_leaf(cwd).to_string(),
        TabLabel::Process(title) => title.to_string(),
        TabLabel::Unknown => "-".to_string(),
    }
}

/// The last segment of a path, for columns that have room for a word and not
/// for a path. Both separators: the same server answers a Windows client, and
/// `C:\proj` has to lose its head too.
pub fn path_leaf(path: &str) -> &str {
    let trimmed = path.trim_end_matches(['/', '\\']);
    match trimmed.rsplit(['/', '\\']).next() {
        Some(leaf) if !leaf.is_empty() => leaf,
        _ => path,
    }
}

/// Display columns, not bytes: a CJK path is two columns per char and three
/// bytes, so padding by `len()` would push every later column out of line.
fn width(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

pub fn table(header: &[&str], rows: &[Vec<String>]) -> String {
    let mut widths: Vec<usize> = header.iter().map(|h| width(h)).collect();
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            if i < widths.len() {
                widths[i] = widths[i].max(width(cell));
            }
        }
    }
    let mut out = String::new();
    render_row(&mut out, &mut header.iter().map(|h| h.to_string()), &widths);
    for row in rows {
        render_row(&mut out, &mut row.iter().cloned(), &widths);
    }
    out
}

fn render_row(out: &mut String, cells: &mut dyn Iterator<Item = String>, widths: &[usize]) {
    let mut line = String::new();
    for (i, cell) in cells.enumerate() {
        if i > 0 {
            line.push_str("  ");
        }
        line.push_str(&cell);
        let pad = widths
            .get(i)
            .copied()
            .unwrap_or(0)
            .saturating_sub(width(&cell));
        line.push_str(&" ".repeat(pad));
    }
    out.push_str(line.trim_end());
    out.push('\n');
}

pub fn workspace_table(machine: &Machine) -> String {
    if machine.workspaces.is_empty() {
        return "no workspaces — `tty7 new <path>` starts one\n".to_string();
    }
    let rows: Vec<Vec<String>> = machine
        .workspaces
        .iter()
        .map(|ws| {
            vec![
                resolve::short_id(&ws.id),
                ws.name.clone().unwrap_or_else(|| "-".to_string()),
                ws.tabs.len().to_string(),
                ws.tabs
                    .iter()
                    .map(|t| t.root.pane_ids().len())
                    .sum::<usize>()
                    .to_string(),
                ws.attachment
                    .as_ref()
                    .map(|a| a.hostname.clone())
                    .unwrap_or_else(|| "-".to_string()),
            ]
        })
        .collect();
    table(&["WORKSPACE", "NAME", "TABS", "PANES", "ATTACHED"], &rows)
}

pub fn pane_table(machine: &Machine, only: Option<WorkspaceId>) -> String {
    let mut rows = Vec::new();
    for ws in &machine.workspaces {
        if only.is_some_and(|id| id != ws.id) {
            continue;
        }
        for tab in &ws.tabs {
            let ordinal = resolve::ordinal_of(machine, tab.id).unwrap_or(0);
            for pane in tab.root.pane_ids() {
                let record = machine.panes.iter().find(|p| p.id == pane);
                rows.push(vec![
                    format!("%{pane}"),
                    ws.name.clone().unwrap_or_else(|| resolve::short_id(&ws.id)),
                    format!("@{ordinal}"),
                    record
                        .and_then(|r| r.cwd.clone())
                        .unwrap_or_else(|| "-".to_string()),
                    record
                        .map(|r| if r.live { "yes" } else { "no" }.to_string())
                        .unwrap_or_else(|| "?".to_string()),
                ]);
            }
        }
    }
    if rows.is_empty() {
        return "no panes\n".to_string();
    }
    table(&["PANE", "WS", "TAB", "CWD", "LIVE"], &rows)
}

/// The server's registry rather than the machine tree, so orphans appear. `held`
/// answers which workspace holds a pane, if any; a pane with no holder is shown
/// as `-` under WS, which is the whole point of the listing.
///
/// OWNER names the workspace allowed to attach to the pane, and for a pane in
/// its own workspace's tree that is the WS beside it — so the column only
/// speaks when the two disagree, which is the case worth reading: an orphan
/// still remembering where it belongs, or a pane the holder cannot attach to.
pub fn registry_table(panes: &[PaneInfo], held: &dyn Fn(u64) -> Option<String>) -> String {
    if panes.is_empty() {
        return "no panes\n".to_string();
    }
    let short = |id: &str| -> String { id.chars().take(8).collect() };
    let rows: Vec<Vec<String>> = panes
        .iter()
        .map(|info| {
            let holder = held(info.pane_id);
            let owner = match (&info.owner, &holder) {
                (None, _) => "-".to_string(),
                (Some(owner), Some(holder)) if owner == holder => "-".to_string(),
                (Some(owner), _) => short(owner),
            };
            vec![
                format!("%{}", info.pane_id),
                holder
                    .as_deref()
                    .map(short)
                    .unwrap_or_else(|| "-".to_string()),
                owner,
                info.cwd
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "-".to_string()),
                if info.alive { "yes" } else { "no" }.to_string(),
            ]
        })
        .collect();
    table(&["PANE", "WS", "OWNER", "CWD", "LIVE"], &rows)
}

pub fn workspace_tree(ws: &Workspace, machine: &Machine) -> String {
    let mut out = format!(
        "{} ({})\n",
        ws.name.as_deref().unwrap_or("-"),
        resolve::short_id(&ws.id)
    );
    for (tab, view) in ws.tabs.iter().zip(tab_views_of(ws, &machine.panes)) {
        let ordinal = resolve::ordinal_of(machine, tab.id).unwrap_or(0);
        match view.label() {
            TabLabel::Unknown => out.push_str(&format!("  @{ordinal}\n")),
            _ => out.push_str(&format!("  @{ordinal}  {}\n", tab_label(&view))),
        }
        render_node(&mut out, &tab.root, machine, 2);
    }
    out
}

fn render_node(out: &mut String, node: &PaneNode, machine: &Machine, depth: usize) {
    let indent = "  ".repeat(depth);
    match node {
        PaneNode::Leaf { pane } => {
            let cwd = machine
                .panes
                .iter()
                .find(|p| p.id == *pane)
                .and_then(|p| p.cwd.clone());
            match cwd {
                Some(cwd) => out.push_str(&format!("{indent}%{pane}  {cwd}\n")),
                None => out.push_str(&format!("{indent}%{pane}\n")),
            }
        }
        PaneNode::Split { axis, ratio, a, b } => {
            let axis = match axis {
                tty7_core::core::machine::Axis::Horizontal => "h",
                tty7_core::core::machine::Axis::Vertical => "v",
            };
            out.push_str(&format!("{indent}{axis} {:.0}%\n", ratio * 100.0));
            render_node(out, a, machine, depth + 1);
            render_node(out, b, machine, depth + 1);
        }
    }
}

pub fn procs_tables(procs: &PaneProcs) -> String {
    if procs.procs.is_empty() && procs.ports.is_empty() {
        return "nothing running in this pane\n".to_string();
    }
    let rows: Vec<Vec<String>> = procs
        .procs
        .iter()
        .map(|p| {
            vec![
                p.pid.to_string(),
                format!("{}{}", "  ".repeat(p.depth as usize), p.name),
                if p.foreground { "*" } else { "" }.to_string(),
            ]
        })
        .collect();
    let mut out = table(&["PID", "NAME", "FG"], &rows);
    if !procs.ports.is_empty() {
        out.push('\n');
        let rows: Vec<Vec<String>> = procs
            .ports
            .iter()
            .map(|p| vec![p.port.to_string(), p.pid.to_string(), p.name.clone()])
            .collect();
        out.push_str(&table(&["PORT", "PID", "NAME"], &rows));
    }
    out
}

pub fn agents_table(states: &[PaneAgentState]) -> String {
    if states.is_empty() {
        return "no agents running\n".to_string();
    }
    let rows: Vec<Vec<String>> = states
        .iter()
        .map(|s| {
            vec![
                format!("%{}", s.pane_id),
                s.agent
                    .map(|a| format!("{a:?}").to_lowercase())
                    .unwrap_or_else(|| "-".to_string()),
                format!("{:?}", s.state.status).to_lowercase(),
                s.state.message.clone().unwrap_or_else(|| "-".to_string()),
            ]
        })
        .collect();
    table(&["PANE", "AGENT", "STATUS", "MESSAGE"], &rows)
}

pub fn status_lines(status: &ServerStatus) -> String {
    let rows = vec![
        vec!["pid".to_string(), status.pid.to_string()],
        vec!["uptime".to_string(), format!("{}s", status.uptime_secs)],
        vec!["panes".to_string(), status.panes.to_string()],
        vec![
            "dialect".to_string(),
            format!(
                "control v{}, protocol v{}",
                status.control_version, status.protocol_version
            ),
        ],
        vec!["build".to_string(), status.build.clone()],
        vec!["socket".to_string(), status.socket.clone()],
    ];
    table(&["SERVER", ""], &rows)
}

pub fn routes_table(routes: &[RouteInfo]) -> String {
    let mut rows = vec![vec![
        "local".to_string(),
        "local".to_string(),
        "yes".to_string(),
    ]];
    rows.extend(routes.iter().map(|r| {
        vec![
            r.key.clone(),
            r.kind.clone(),
            if r.connected { "yes" } else { "no" }.to_string(),
        ]
    }));
    table(&["MACHINE", "KIND", "CONNECTED"], &rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testbed::two_workspace_machine;
    use tty7_core::daemon::protocol::{PortEntry, ProcEntry};

    #[test]
    fn columns_line_up_and_lines_carry_no_trailing_spaces() {
        let rendered = table(
            &["PANE", "CWD"],
            &[
                vec!["%1".into(), "C:\\proj".into()],
                vec!["%12345".into(), "C:\\".into()],
            ],
        );
        assert_eq!(rendered, "PANE    CWD\n%1      C:\\proj\n%12345  C:\\\n");
        assert!(
            rendered.lines().all(|l| l == l.trim_end()),
            "trailing spaces break naive downstream parsing"
        );
    }

    #[test]
    fn owner_speaks_only_when_it_disagrees_with_the_workspace_holding_the_pane() {
        let held = "9fd8072f-465c-4016-9a81-8143bff1240c";
        let elsewhere = "76698a44-3f13-4961-8fed-90d0b3defff1";
        let pane = |id: u64, owner: Option<&str>| PaneInfo {
            pane_id: id,
            cwd: None,
            title: "zsh".into(),
            alive: true,
            owner: owner.map(str::to_string),
        };
        let rendered = registry_table(
            &[
                pane(1, Some(held)),
                pane(2, Some(elsewhere)),
                pane(3, None),
                pane(4, Some(elsewhere)),
            ],
            &|id| (id != 4).then(|| held.to_string()),
        );

        let owner_of = |pane: &str| -> String {
            rendered
                .lines()
                .find(|line| line.starts_with(pane))
                .unwrap_or_else(|| panic!("{pane} is listed: {rendered}"))
                .split_whitespace()
                .nth(2)
                .expect("PANE WS OWNER")
                .to_string()
        };
        assert_eq!(
            owner_of("%1"),
            "-",
            "repeating the WS beside it says nothing"
        );
        assert_eq!(
            owner_of("%2"),
            "76698a44",
            "a holder that may not attach is the whole reason to look"
        );
        assert_eq!(owner_of("%3"), "-", "nobody claims it");
        assert_eq!(
            owner_of("%4"),
            "76698a44",
            "an orphan still remembers where it belongs"
        );
    }

    #[test]
    fn wide_characters_are_padded_by_display_width_not_byte_length() {
        // "项目" is 6 bytes but occupies 4 columns. Padding by len() would add
        // 2 spaces too few and skew every column after it.
        let rendered = table(
            &["NAME", "TABS"],
            &[
                vec!["项目".into(), "3".into()],
                vec!["api".into(), "1".into()],
            ],
        );
        // Column one is 4 wide: "NAME" and "项目" both fill it exactly, "api"
        // gets one pad space. Sizing by len() would make it 6 (项目's byte
        // count) and push the header's second column two places right.
        assert_eq!(rendered, "NAME  TABS\n项目  3\napi   1\n");

        // The second column starts at the same display offset on every line.
        let second_column_at: Vec<usize> = rendered
            .lines()
            .map(|line| {
                let split = line.rfind(' ').expect("two columns per line") + 1;
                UnicodeWidthStr::width(&line[..split])
            })
            .collect();
        assert!(
            second_column_at.iter().all(|c| *c == second_column_at[0]),
            "columns must line up on screen: {rendered:?} gave {second_column_at:?}"
        );
    }

    #[test]
    fn the_workspace_table_is_one_line_per_workspace() {
        let m = two_workspace_machine();
        let rendered = workspace_table(&m);
        let lines: Vec<&str> = rendered.lines().collect();
        assert_eq!(lines.len(), 3, "header plus two workspaces:\n{rendered}");
        assert!(lines[0].starts_with("WORKSPACE"));
        assert!(lines[1].contains("api"), "{rendered}");
        assert!(lines[1].contains('3'), "api holds three panes: {rendered}");
        assert!(lines[2].contains("web"), "{rendered}");
    }

    #[test]
    fn an_empty_machine_says_how_to_start() {
        let rendered = workspace_table(&Machine::default());
        assert!(rendered.contains("tty7 new"), "{rendered}");
    }

    #[test]
    fn the_pane_table_addresses_panes_and_tabs_the_way_commands_take_them() {
        let m = two_workspace_machine();
        let rendered = pane_table(&m, None);
        assert!(rendered.contains("%1"), "{rendered}");
        assert!(rendered.contains("%5"), "{rendered}");
        assert!(
            rendered.contains("@3"),
            "web's tab keeps its machine-wide number: {rendered}"
        );

        let web_only = pane_table(&m, Some(m.workspaces[1].id));
        assert!(!web_only.contains("%1"), "{web_only}");
        assert!(web_only.contains("%5"), "{web_only}");
    }

    #[test]
    fn the_tree_shows_tabs_splits_and_cwds_by_indentation() {
        let m = two_workspace_machine();
        let rendered = workspace_tree(&m.workspaces[0], &m);
        // @1 was named; @2 was not, so it borrows the leaf of its cwd rather
        // than printing nothing at all.
        let expected = format!(
            "api ({})\n  @1  build\n    %1  C:\\proj\n  @2  proj\n    h 50%\n      %2  C:\\proj\n      %3  C:\\proj\\sub\n",
            crate::resolve::short_id(&m.workspaces[0].id)
        );
        assert_eq!(rendered, expected);
    }

    #[test]
    fn an_unnamed_tab_borrows_an_agent_then_a_place_then_its_process() {
        let view = |f: &dyn Fn(&mut TabView)| {
            let mut v = TabView {
                id: tty7_core::core::machine::TabId::new(),
                name: None,
                title: String::new(),
                cwd: None,
                agent: None,
                status: None,
                live: true,
                panes: 1,
            };
            f(&mut v);
            v
        };
        assert_eq!(
            tab_label(&view(&|v| v.name = Some("deploy".into()))),
            "deploy"
        );
        assert_eq!(
            tab_label(&view(
                &|v| v.agent = Some(tty7_core::core::cli_agent::CLIAgent::Claude)
            )),
            "Claude Code"
        );
        assert_eq!(
            tab_label(&view(&|v| v.cwd = Some("/Users/me/repo/tty7".into()))),
            "tty7"
        );
        assert_eq!(
            tab_label(&view(&|v| v.cwd = Some("C:\\proj\\sub\\".into()))),
            "sub",
            "a Windows path loses its head and its trailing separator"
        );
        assert_eq!(tab_label(&view(&|v| v.cwd = Some("/".into()))), "/");
        assert_eq!(tab_label(&view(&|v| v.title = "zsh".into())), "zsh");
        assert_eq!(tab_label(&view(&|_| {})), "-");
    }

    #[test]
    fn procs_render_as_a_process_tree_plus_ports() {
        let procs = PaneProcs {
            procs: vec![
                ProcEntry {
                    pid: 100,
                    name: "pwsh".into(),
                    depth: 0,
                    foreground: false,
                },
                ProcEntry {
                    pid: 200,
                    name: "cargo".into(),
                    depth: 1,
                    foreground: true,
                },
            ],
            ports: vec![PortEntry {
                port: 3000,
                pid: 200,
                addr: "*".into(),
                name: "node".into(),
            }],
        };
        let rendered = procs_tables(&procs);
        assert!(
            rendered.contains("  cargo"),
            "children are indented: {rendered}"
        );
        assert!(
            rendered.contains('*'),
            "the foreground process is marked: {rendered}"
        );
        assert!(rendered.contains("3000"), "{rendered}");
    }
}
