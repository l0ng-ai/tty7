use gpui::{
    App, Context, Entity, EventEmitter, MouseButton, MouseDownEvent, SharedString, Subscription,
    Task, Window, div, prelude::*, px,
};
use gpui_component::{
    ActiveTheme as _, IndexPath, h_flex,
    list::{List, ListDelegate, ListEvent, ListItem, ListState},
    v_flex,
};

use uuid::Uuid;

use crate::core::config::{Config, RightPanelTab, TabBarPosition};
use crate::core::ssh_profile::parse_quick_connect;

#[derive(Clone, PartialEq, Eq)]
pub enum CommandKind {
    NewTab,
    NewWorkspace,
    OpenWorkspacePicker,
    RenameWorkspace,
    StopWorkspace,
    DeleteWorkspace,
    SplitRight,
    SplitDown,
    ClosePane,
    RenameTab,
    NewWorktreeTab,
    CloseOtherTabs,
    CloseTabsToTheRight,
    CopyWorkingDirectory,
    MarkTabUnread,
    ForkAgentSession,
    CopyAgentSessionId,
    ResetFontSize,
    NextPane,
    PrevPane,
    FocusPaneLeft,
    FocusPaneRight,
    FocusPaneUp,
    FocusPaneDown,
    ResizePaneLeft,
    ResizePaneRight,
    ResizePaneUp,
    ResizePaneDown,
    SwapPaneNext,
    SwapPanePrev,
    NextTab,
    PrevTab,
    ToggleMaximizePane,
    ToggleFullscreen,
    ToggleTabSidebar,
    ToggleLeftPanel,
    ToggleRightPanel,
    ShowRightPanel(RightPanelTab),
    ClearTerminal,
    FindInTerminal,
    FindNext,
    FindPrevious,
    CopyText,
    CutText,
    PasteText,
    SelectAllText,
    ReopenClosedTab,
    OpenSettings,
    ShowKeyboardShortcuts,
    About,
    CheckForUpdates,
    OpenDocumentation,
    OpenDiscord,
    ReportIssue,
    Quit,
    RestartDaemon,
    ToggleSftp,
    ShowSshForwards,
    ToggleCodePanel,
    RestartSshSession,
    SendSelectionToAgent,
    SendGitDiffToAgent,
    OpenThemePicker,
    OpenSshConnectInput,
    OpenSshConnect(String),
    SetTheme(usize),
    ActivateTab(usize),
    ConnectSavedProfile(Uuid),
    EditSavedProfile(Uuid),
    QuickConnect(String),
    SaveQuickConnect(String),
    OpenSshProfiles,
}

impl CommandKind {
    pub fn edit_variant(&self) -> Option<CommandKind> {
        match self {
            CommandKind::ConnectSavedProfile(id) => Some(CommandKind::EditSavedProfile(*id)),
            CommandKind::QuickConnect(s) => Some(CommandKind::SaveQuickConnect(s.clone())),
            _ => None,
        }
    }

    pub fn id(&self) -> Option<&'static str> {
        use CommandKind::*;
        Some(match self {
            NewTab => "new-tab",
            NewWorkspace => "new-workspace",
            OpenWorkspacePicker => "switch-workspace",
            RenameWorkspace => "rename-workspace",
            StopWorkspace => "stop-workspace",
            DeleteWorkspace => "delete-workspace",
            SplitRight => "split-right",
            SplitDown => "split-down",
            ClosePane => "close-pane",
            RenameTab => "rename-tab",
            NewWorktreeTab => "new-worktree-tab",
            CloseOtherTabs => "close-other-tabs",
            CloseTabsToTheRight => "close-tabs-right",
            CopyWorkingDirectory => "copy-cwd",
            MarkTabUnread => "mark-tab-unread",
            ForkAgentSession => "fork-agent-session",
            CopyAgentSessionId => "copy-agent-session-id",
            ResetFontSize => "reset-font-size",
            NextPane => "next-pane",
            PrevPane => "prev-pane",
            FocusPaneLeft => "focus-pane-left",
            FocusPaneRight => "focus-pane-right",
            FocusPaneUp => "focus-pane-up",
            FocusPaneDown => "focus-pane-down",
            ResizePaneLeft => "resize-pane-left",
            ResizePaneRight => "resize-pane-right",
            ResizePaneUp => "resize-pane-up",
            ResizePaneDown => "resize-pane-down",
            SwapPaneNext => "swap-pane-next",
            SwapPanePrev => "swap-pane-prev",
            NextTab => "next-tab",
            PrevTab => "prev-tab",
            ToggleMaximizePane => "zoom-pane",
            ToggleFullscreen => "full-screen",
            ToggleTabSidebar => "tab-bar-position",
            ToggleLeftPanel => "left-sidebar",
            ToggleRightPanel => "right-panel",
            ShowRightPanel(RightPanelTab::Info) => "right-panel-info",
            ShowRightPanel(RightPanelTab::Outline) => "right-panel-outline",
            ShowRightPanel(RightPanelTab::Changes) => "right-panel-changes",
            ShowRightPanel(RightPanelTab::Files) => "right-panel-files",
            ClearTerminal => "clear-scrollback",
            FindInTerminal => "find",
            FindNext => "find-next",
            FindPrevious => "find-previous",
            CopyText => "copy",
            CutText => "cut",
            PasteText => "paste",
            SelectAllText => "select-all",
            ReopenClosedTab => "reopen-closed-tab",
            OpenSettings => "settings",
            ShowKeyboardShortcuts => "keyboard-shortcuts",
            About => "about",
            CheckForUpdates => "check-for-updates",
            OpenDocumentation => "documentation",
            OpenDiscord => "discord",
            ReportIssue => "report-issue",
            Quit => "quit",
            RestartDaemon => "restart-daemon",
            ToggleSftp => "ssh-remote-files",
            ShowSshForwards => "ssh-port-forwarding",
            ToggleCodePanel => "code-panel",
            RestartSshSession => "ssh-reconnect",
            SendSelectionToAgent => "agent-send-selection",
            SendGitDiffToAgent => "agent-send-diff",
            OpenThemePicker => "change-theme",
            OpenSshConnectInput => "ssh-add-connection",
            OpenSshProfiles => "ssh-manage-profiles",
            OpenSshConnect(_)
            | SetTheme(_)
            | ActivateTab(_)
            | ConnectSavedProfile(_)
            | EditSavedProfile(_)
            | QuickConnect(_)
            | SaveQuickConnect(_) => return None,
        })
    }

    fn key_spec(&self, cx: &App) -> Option<String> {
        use CommandKind::*;
        let inline =
            |spec: &str| -> Option<String> { cfg!(target_os = "macos").then(|| spec.to_string()) };
        match self {
            CopyText => return inline("secondary-c"),
            CutText => return inline("secondary-x"),
            PasteText => return inline("secondary-v"),
            SelectAllText => return inline("secondary-a"),
            _ => {}
        }
        let action = match self {
            NewTab => "NewTab",
            NewWorkspace => "NewWorkspace",
            RenameWorkspace => "RenameWorkspace",
            StopWorkspace => "StopWorkspace",
            DeleteWorkspace => "DeleteWorkspace",
            SplitRight => "SplitRight",
            SplitDown => "SplitDown",
            ClosePane => "CloseActiveTab",
            RenameTab => "RenameTab",
            NewWorktreeTab => "NewWorktreeTab",
            CloseOtherTabs => "CloseOtherTabs",
            CloseTabsToTheRight => "CloseTabsToTheRight",
            CopyWorkingDirectory => "CopyWorkingDirectory",
            MarkTabUnread => "MarkTabUnread",
            ForkAgentSession => "ForkAgentSession",
            CopyAgentSessionId => "CopyAgentSessionId",
            ResetFontSize => "ResetFontSize",
            NextPane => "FocusNextPane",
            PrevPane => "FocusPrevPane",
            FocusPaneLeft => "FocusPaneLeft",
            FocusPaneRight => "FocusPaneRight",
            FocusPaneUp => "FocusPaneUp",
            FocusPaneDown => "FocusPaneDown",
            ResizePaneLeft => "ResizePaneLeft",
            ResizePaneRight => "ResizePaneRight",
            ResizePaneUp => "ResizePaneUp",
            ResizePaneDown => "ResizePaneDown",
            SwapPaneNext => "SwapPaneNext",
            SwapPanePrev => "SwapPanePrev",
            NextTab => "NextTab",
            PrevTab => "PrevTab",
            ToggleMaximizePane => "ToggleMaximizePane",
            ToggleFullscreen => "ToggleFullscreen",
            ToggleTabSidebar => "ToggleTabSidebar",
            ToggleLeftPanel => "ToggleLeftPanel",
            ToggleRightPanel => "ToggleRightPanel",
            ShowRightPanel(tab) => match tab {
                RightPanelTab::Info => "ShowRightPanelInfo",
                RightPanelTab::Outline => "ShowRightPanelOutline",
                RightPanelTab::Changes => "ShowRightPanelChanges",
                RightPanelTab::Files => "ShowRightPanelFiles",
            },
            ClearTerminal => "ClearScrollback",
            FindInTerminal => "FindInTerminal",
            FindNext => "FindNext",
            FindPrevious => "FindPrevious",
            ReopenClosedTab => "ReopenClosedTab",
            OpenSettings => "OpenSettings",
            ShowKeyboardShortcuts => "ShowKeyboardShortcuts",
            About => "About",
            CheckForUpdates => "CheckForUpdates",
            OpenDocumentation => "OpenDocumentation",
            OpenDiscord => "OpenDiscord",
            ReportIssue => "ReportIssue",
            Quit => "Quit",
            RestartDaemon => "RestartDaemon",
            ToggleSftp => "ToggleSftp",
            ShowSshForwards => "ShowSshForwards",
            ToggleCodePanel => "ToggleCodePanel",
            RestartSshSession => "RestartSshSession",
            OpenSshProfiles => "OpenSshProfiles",
            CopyText
            | CutText
            | PasteText
            | SelectAllText
            | SendSelectionToAgent
            | SendGitDiffToAgent
            | OpenWorkspacePicker
            | OpenThemePicker
            | OpenSshConnectInput
            | OpenSshConnect(_)
            | SetTheme(_)
            | ActivateTab(_)
            | ConnectSavedProfile(_)
            | EditSavedProfile(_)
            | QuickConnect(_)
            | SaveQuickConnect(_) => return None,
        };
        crate::ui::keymap::effective_key(action, cx)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CommandGroup {
    TabsPanes,
    Workspaces,
    View,
    Terminal,
    Ssh,
    Agents,
    Application,
}

impl CommandGroup {
    const ORDER: [CommandGroup; 7] = [
        CommandGroup::TabsPanes,
        CommandGroup::Workspaces,
        CommandGroup::View,
        CommandGroup::Terminal,
        CommandGroup::Ssh,
        CommandGroup::Agents,
        CommandGroup::Application,
    ];

    fn title(self) -> &'static str {
        match self {
            CommandGroup::TabsPanes => "Tabs & Panes",
            CommandGroup::Workspaces => "Workspaces",
            CommandGroup::View => "View",
            CommandGroup::Terminal => "Terminal",
            CommandGroup::Ssh => "SSH",
            CommandGroup::Agents => "Agents",
            CommandGroup::Application => "Application",
        }
    }
}

#[derive(Clone, Copy)]
pub struct ChromeState {
    pub rail_collapsed: bool,
    pub right_panel_visible: bool,
}

#[derive(Clone)]
pub struct Command {
    pub title: String,
    pub subtitle: Option<String>,
    pub kind: CommandKind,
    pub group: CommandGroup,
}

impl Command {
    pub fn new(title: impl Into<String>, kind: CommandKind) -> Self {
        Self {
            title: title.into(),
            subtitle: None,
            kind,
            group: CommandGroup::Application,
        }
    }

    pub fn with_subtitle(mut self, subtitle: impl Into<String>) -> Self {
        self.subtitle = Some(subtitle.into());
        self
    }

    pub fn in_group(mut self, group: CommandGroup) -> Self {
        self.group = group;
        self
    }

    pub fn base_commands(cx: &App, chrome: ChromeState) -> Vec<Command> {
        use CommandKind::*;
        let cfg = cx.global::<Config>();
        let tab_bar_left = cfg.tab_bar_position == TabBarPosition::Left;
        let sidebar_hidden = chrome.rail_collapsed || !tab_bar_left;
        let right_panel_open = chrome.right_panel_visible;

        let tabs = [
            Command::new("New Tab", NewTab),
            Command::new("New Worktree Tab", NewWorktreeTab)
                .with_subtitle("isolated checkout on a fresh branch"),
            Command::new("Rename Tab…", RenameTab),
            Command::new("Split Right", SplitRight),
            Command::new("Split Down", SplitDown),
            Command::new("Zoom Pane", ToggleMaximizePane),
            Command::new("Next Pane", NextPane),
            Command::new("Previous Pane", PrevPane),
            Command::new("Focus Pane Left", FocusPaneLeft),
            Command::new("Focus Pane Right", FocusPaneRight),
            Command::new("Focus Pane Up", FocusPaneUp),
            Command::new("Focus Pane Down", FocusPaneDown),
            Command::new("Resize Pane Left", ResizePaneLeft),
            Command::new("Resize Pane Right", ResizePaneRight),
            Command::new("Resize Pane Up", ResizePaneUp),
            Command::new("Resize Pane Down", ResizePaneDown),
            Command::new("Swap Pane Next", SwapPaneNext),
            Command::new("Swap Pane Previous", SwapPanePrev),
            Command::new("Next Tab", NextTab),
            Command::new("Previous Tab", PrevTab),
            Command::new("Copy Working Directory", CopyWorkingDirectory),
            Command::new("Copy Session ID", CopyAgentSessionId)
                .with_subtitle("the coding agent's own session id"),
            Command::new("Fork Session", ForkAgentSession)
                .with_subtitle("branch this agent session into a new tab"),
            Command::new("Mark Tab as Unread", MarkTabUnread),
            Command::new("Close Pane / Tab", ClosePane),
            Command::new("Close Other Tabs", CloseOtherTabs),
            Command::new("Close Tabs to the Right", CloseTabsToTheRight),
            Command::new("Reopen Closed Tab", ReopenClosedTab),
        ];

        let workspaces = [
            Command::new("New Workspace", NewWorkspace),
            Command::new("Switch Workspace…", OpenWorkspacePicker),
            Command::new("Rename Workspace…", RenameWorkspace),
            Command::new("Stop Workspace…", StopWorkspace)
                .with_subtitle("ends its sessions, keeps the layout"),
            Command::new("Delete Workspace…", DeleteWorkspace)
                .with_subtitle("ends its sessions and forgets the layout"),
        ];

        let view = [
            Command::new(
                if sidebar_hidden {
                    "Show Left Sidebar"
                } else {
                    "Hide Left Sidebar"
                },
                ToggleLeftPanel,
            ),
            Command::new(
                if right_panel_open {
                    "Hide Right Panel"
                } else {
                    "Show Right Panel"
                },
                ToggleRightPanel,
            ),
            Command::new("Show Code Panel", ToggleCodePanel),
            Command::new(
                if tab_bar_left {
                    "Tab Bar: Move to Top"
                } else {
                    "Tab Bar: Move to Left Sidebar"
                },
                ToggleTabSidebar,
            ),
            Command::new("Right Panel: Info", ShowRightPanel(RightPanelTab::Info)),
            Command::new(
                "Right Panel: Outline",
                ShowRightPanel(RightPanelTab::Outline),
            ),
            Command::new(
                "Right Panel: Changes",
                ShowRightPanel(RightPanelTab::Changes),
            ),
            Command::new("Right Panel: Files", ShowRightPanel(RightPanelTab::Files)),
            Command::new("Change Theme…", OpenThemePicker),
            Command::new("Reset Font Size", ResetFontSize),
            Command::new("Enter Full Screen", ToggleFullscreen),
        ];

        let terminal = [
            Command::new("Clear Scrollback", ClearTerminal),
            Command::new("Find in Terminal…", FindInTerminal),
            Command::new("Find Next", FindNext),
            Command::new("Find Previous", FindPrevious),
            Command::new("Copy", CopyText),
            Command::new("Cut", CutText),
            Command::new("Paste", PasteText),
            Command::new("Select All", SelectAllText),
        ];

        let ssh = [
            Command::new("SSH: Add Connection…", OpenSshConnectInput),
            Command::new("SSH: Manage Profiles…", OpenSshProfiles),
            Command::new("SSH: Reconnect Session", RestartSshSession),
            Command::new("SSH: Remote Files", ToggleSftp),
            Command::new("SSH: Port Forwarding", ShowSshForwards),
        ];

        let agents = [
            Command::new("Agent: Send Selection", SendSelectionToAgent)
                .with_subtitle("selection → running coding agent"),
            Command::new("Agent: Send Git Diff for Review", SendGitDiffToAgent)
                .with_subtitle("git diff → running coding agent"),
        ];

        let application = [
            Command::new("Settings…", OpenSettings),
            Command::new("Keyboard Shortcuts", ShowKeyboardShortcuts),
            Command::new("About tty7", About),
            Command::new("Check for Updates…", CheckForUpdates),
            Command::new("Documentation", OpenDocumentation),
            Command::new("Join the Discord", OpenDiscord),
            Command::new("Report an Issue…", ReportIssue),
            Command::new("Restart Daemon…", RestartDaemon)
                .with_subtitle("ends every running shell; layout is kept"),
            Command::new("Quit tty7", Quit).with_subtitle("sessions keep running"),
        ];

        let mut out = Vec::new();
        let mut push = |cmds: Vec<Command>, group: CommandGroup| {
            out.extend(cmds.into_iter().map(|c| c.in_group(group)));
        };
        push(tabs.into(), CommandGroup::TabsPanes);
        push(workspaces.into(), CommandGroup::Workspaces);
        push(view.into(), CommandGroup::View);
        push(terminal.into(), CommandGroup::Terminal);
        push(ssh.into(), CommandGroup::Ssh);
        push(agents.into(), CommandGroup::Agents);
        push(application.into(), CommandGroup::Application);
        out
    }

    pub fn theme_commands(cx: &App) -> Vec<Command> {
        let active = crate::ui::theme::effective_preset_id(cx);
        crate::ui::presets::all(cx)
            .into_iter()
            .enumerate()
            .map(|(i, p)| {
                let title = if p.id == active {
                    format!("{}  ✓", p.name)
                } else {
                    p.name.clone()
                };
                Command::new(title, CommandKind::SetTheme(i))
            })
            .collect()
    }

    fn ssh_connect_command(input: &str) -> Command {
        let title = if input.trim().is_empty() {
            "SSH: Add Connection…".to_string()
        } else {
            format!("SSH: Connect {}", input.trim())
        };
        Command::new(title, CommandKind::OpenSshConnect(input.to_string()))
    }
}

pub fn fuzzy_score(query: &str, text: &str) -> Option<i32> {
    let needle: Vec<char> = query
        .chars()
        .flat_map(char::to_lowercase)
        .filter(|c| !c.is_whitespace())
        .collect();
    if needle.is_empty() {
        return Some(0);
    }
    let hay: Vec<char> = text.chars().flat_map(char::to_lowercase).collect();
    if needle.len() > hay.len() {
        return None;
    }

    let mut qi = 0usize;
    let mut score = 0i32;
    let mut run = 0i32;
    let mut prev_hit = false;
    for (i, ch) in hay.iter().enumerate() {
        if qi >= needle.len() {
            break;
        }
        if *ch != needle[qi] {
            prev_hit = false;
            run = 0;
            continue;
        }
        score += 1;
        let word_start = i == 0 || !hay[i - 1].is_alphanumeric();
        if word_start {
            score += 12;
        }
        if i == 0 {
            score += 10;
        }
        if prev_hit {
            run += 1;
            score += 6 + run.min(8);
        } else {
            run = 0;
        }
        prev_hit = true;
        qi += 1;
    }
    if qi < needle.len() {
        return None;
    }

    if hay == needle {
        score += 120;
    } else if hay.starts_with(&needle) {
        score += 50;
    }
    score -= (hay.len() as i32) / 6;
    Some(score)
}

fn command_score(query: &str, cmd: &Command) -> Option<i32> {
    let title = fuzzy_score(query, &cmd.title);
    let subtitle = cmd
        .subtitle
        .as_deref()
        .and_then(|s| fuzzy_score(query, s))
        .map(|s| s / 2 - 25);
    match (title, subtitle) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (a, b) => a.or(b),
    }
}

#[derive(Clone)]
struct Section {
    title: Option<SharedString>,
    commands: Vec<Command>,
}

pub struct PaletteDelegate {
    commands: Vec<Command>,
    sections: Vec<Section>,
    input: Option<PaletteInput>,
    quick_connect_root: bool,
    selected: Option<IndexPath>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PaletteInput {
    SshConnect,
}

impl PaletteDelegate {
    pub fn new(commands: Vec<Command>) -> Self {
        Self {
            sections: vec![Section {
                title: None,
                commands: commands.clone(),
            }],
            commands,
            input: None,
            quick_connect_root: false,
            selected: Some(IndexPath::default()),
        }
    }

    pub fn root(commands: Vec<Command>, cx: &App) -> Self {
        let mut this = Self {
            quick_connect_root: true,
            ..Self::new(commands)
        };
        this.sections = this.grouped_sections(cx);
        this
    }

    fn grouped_sections(&self, cx: &App) -> Vec<Section> {
        let cfg = cx.global::<Config>();
        let now = crate::core::config::unix_now();
        let mut sections = Vec::new();

        let mut recent: Vec<(f64, &Command)> = self
            .commands
            .iter()
            .filter_map(|c| {
                let id = c.kind.id()?;
                let usage = cfg.command_frecency.get(id)?;
                let score = usage.score(now);
                (score > 0.0).then_some((score, c))
            })
            .collect();
        recent.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        recent.truncate(RECENT_ROWS);
        if !recent.is_empty() {
            sections.push(Section {
                title: Some("Recent".into()),
                commands: recent.into_iter().map(|(_, c)| c.clone()).collect(),
            });
        }

        for group in CommandGroup::ORDER {
            let commands: Vec<Command> = self
                .commands
                .iter()
                .filter(|c| c.group == group)
                .cloned()
                .collect();
            if !commands.is_empty() {
                sections.push(Section {
                    title: Some(group.title().into()),
                    commands,
                });
            }
        }
        sections
    }

    fn quick_connect_commands(query: &str) -> Vec<Command> {
        if !query.contains(['@', ':', '.']) {
            return Vec::new();
        }
        match parse_quick_connect(query) {
            Some(_) => {
                let target = query.trim().to_string();
                vec![
                    Command::new(
                        format!("Connect to \"{target}\""),
                        CommandKind::QuickConnect(target.clone()),
                    ),
                    Command::new(
                        format!("Save \"{target}\" as profile…"),
                        CommandKind::SaveQuickConnect(target),
                    ),
                ]
            }
            None => Vec::new(),
        }
    }

    fn ssh_connect() -> Self {
        Self {
            commands: Vec::new(),
            sections: vec![Section {
                title: None,
                commands: vec![Command::ssh_connect_command("")],
            }],
            input: Some(PaletteInput::SshConnect),
            quick_connect_root: false,
            selected: Some(IndexPath::default()),
        }
    }

    pub fn command_at(&self, ix: IndexPath) -> Option<CommandKind> {
        self.sections
            .get(ix.section)?
            .commands
            .get(ix.row)
            .map(|c| c.kind.clone())
    }

    pub fn selected_command(&self) -> Option<CommandKind> {
        self.selected.and_then(|ix| self.command_at(ix))
    }

    fn first_row(&self) -> Option<IndexPath> {
        let section = self.sections.iter().position(|s| !s.commands.is_empty())?;
        Some(IndexPath::new(0).section(section))
    }
}

impl ListDelegate for PaletteDelegate {
    type Item = ListItem;

    fn sections_count(&self, _cx: &App) -> usize {
        self.sections.len().max(1)
    }

    fn items_count(&self, section: usize, _cx: &App) -> usize {
        self.sections
            .get(section)
            .map(|s| s.commands.len())
            .unwrap_or(0)
    }

    fn perform_search(
        &mut self,
        query: &str,
        _window: &mut Window,
        cx: &mut Context<ListState<Self>>,
    ) -> Task<()> {
        if let Some(PaletteInput::SshConnect) = self.input {
            self.sections = vec![Section {
                title: None,
                commands: vec![Command::ssh_connect_command(query)],
            }];
        } else if query.trim().is_empty() {
            self.sections = if self.quick_connect_root {
                self.grouped_sections(cx)
            } else {
                vec![Section {
                    title: None,
                    commands: self.commands.clone(),
                }]
            };
        } else {
            let mut scored: Vec<(i32, Command)> = self
                .commands
                .iter()
                .filter_map(|c| command_score(query, c).map(|s| (s, c.clone())))
                .collect();
            scored.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
            let mut commands: Vec<Command> = Vec::new();
            if self.quick_connect_root {
                commands.extend(Self::quick_connect_commands(query));
            }
            commands.extend(scored.into_iter().map(|(_, c)| c));
            self.sections = vec![Section {
                title: None,
                commands,
            }];
        }
        self.selected = self.first_row();
        Task::ready(())
    }

    fn render_section_header(
        &mut self,
        section: usize,
        _window: &mut Window,
        cx: &mut Context<ListState<Self>>,
    ) -> Option<impl IntoElement> {
        let title = self.sections.get(section)?.title.clone()?;
        Some(
            h_flex()
                .h(px(PALETTE_ROW_H))
                .px(px(11.))
                .items_center()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child(title),
        )
    }

    fn render_empty(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<ListState<Self>>,
    ) -> impl IntoElement {
        v_flex()
            .py_8()
            .gap_1()
            .items_center()
            .text_sm()
            .text_color(cx.theme().muted_foreground)
            .child("No matching commands")
            .child(
                div()
                    .text_xs()
                    .child("Type user@host to connect over SSH instead."),
            )
    }

    fn render_item(
        &mut self,
        ix: IndexPath,
        _window: &mut Window,
        cx: &mut Context<ListState<Self>>,
    ) -> Option<Self::Item> {
        let cmd = self.sections.get(ix.section)?.commands.get(ix.row)?.clone();

        let (kbd_bg, border, muted) = {
            let t = cx.theme();
            (t.secondary.opacity(0.6), t.border, t.muted_foreground)
        };

        let keys = cmd
            .kind
            .key_spec(cx)
            .map(|spec| crate::ui::keymap::key_tokens(&spec));

        let mut left = h_flex().items_center().gap_2().child(cmd.title.clone());
        if let Some(subtitle) = cmd.subtitle.clone() {
            left = left.child(div().text_xs().text_color(muted).child(subtitle));
        }

        let mut row = h_flex()
            .w_full()
            .items_center()
            .justify_between()
            .child(left);
        if cmd.kind.edit_variant().is_some() {
            row = row.child(div().text_xs().text_color(muted).child("→ edit"));
        }
        if let Some(tokens) = keys {
            row = row.child(h_flex().gap_1().children(tokens.into_iter().map(move |t| {
                div()
                    .flex()
                    .items_center()
                    .justify_center()
                    .min_w(px(20.))
                    .h(px(20.))
                    .px_1()
                    .rounded_md()
                    .bg(kbd_bg)
                    .border_1()
                    .border_color(border)
                    .text_xs()
                    .text_color(muted)
                    .child(t)
            })));
        }

        Some(
            ListItem::new(("palette-row", ix.section * 1000 + ix.row))
                .selected(Some(ix) == self.selected)
                .h(px(PALETTE_ROW_H))
                .mx(px(5.))
                .rounded(px(6.))
                .text_sm()
                .child(row),
        )
    }

    fn set_selected_index(
        &mut self,
        ix: Option<IndexPath>,
        _window: &mut Window,
        cx: &mut Context<ListState<Self>>,
    ) {
        self.selected = ix;
        cx.notify();
    }
}

pub enum PaletteEvent {
    Confirm(CommandKind),
    Dismiss,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PaletteMenu {
    Root,
    Theme,
    SshConnect,
}

pub struct PaletteView {
    list: Entity<ListState<PaletteDelegate>>,
    root: Vec<Command>,
    menu: PaletteMenu,
    _sub: Subscription,
}

impl PaletteView {
    pub fn new(commands: Vec<Command>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let list = Self::build_root_list(commands.clone(), window, cx);
        let _sub = cx.subscribe_in(&list, window, Self::on_list_event);
        Self {
            list,
            root: commands,
            menu: PaletteMenu::Root,
            _sub,
        }
    }

    fn build_list(
        commands: Vec<Command>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<ListState<PaletteDelegate>> {
        Self::build_list_with_delegate(PaletteDelegate::new(commands), window, cx)
    }

    fn build_root_list(
        commands: Vec<Command>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<ListState<PaletteDelegate>> {
        let delegate = PaletteDelegate::root(commands, cx);
        Self::build_list_with_delegate(delegate, window, cx)
    }

    fn build_list_with_delegate(
        delegate: PaletteDelegate,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<ListState<PaletteDelegate>> {
        let list = cx.new(|cx| ListState::new(delegate, window, cx).searchable(true));
        list.update(cx, |state, cx| state.focus(window, cx));
        list
    }

    fn show(&mut self, commands: Vec<Command>, window: &mut Window, cx: &mut Context<Self>) {
        let list = Self::build_list(commands, window, cx);
        self._sub = cx.subscribe_in(&list, window, Self::on_list_event);
        self.list = list;
        cx.notify();
    }

    fn show_ssh_connect(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let list = Self::build_list_with_delegate(PaletteDelegate::ssh_connect(), window, cx);
        self._sub = cx.subscribe_in(&list, window, Self::on_list_event);
        self.list = list;
        cx.notify();
    }

    fn search_placeholder(&self) -> &'static str {
        match self.menu {
            PaletteMenu::SshConnect => "user@host [-p 2222 -J jump]",
            PaletteMenu::Root => "Search or type user@host to connect…",
            PaletteMenu::Theme => "Search…",
        }
    }

    fn selected_edit_command(&self, cx: &App) -> Option<CommandKind> {
        self.list
            .read(cx)
            .delegate()
            .selected_command()
            .and_then(|k| k.edit_variant())
    }

    fn on_list_event(
        &mut self,
        list: &Entity<ListState<PaletteDelegate>>,
        ev: &ListEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match ev {
            ListEvent::Confirm(ix) => {
                let kind = list.read(cx).delegate().command_at(*ix);
                match kind {
                    Some(CommandKind::OpenThemePicker) => {
                        self.menu = PaletteMenu::Theme;
                        let themes = Command::theme_commands(cx);
                        self.show(themes, window, cx);
                    }
                    Some(CommandKind::OpenSshConnectInput) => {
                        self.menu = PaletteMenu::SshConnect;
                        self.show_ssh_connect(window, cx);
                    }
                    Some(kind @ CommandKind::OpenWorkspacePicker) => {
                        cx.emit(PaletteEvent::Confirm(kind))
                    }
                    Some(CommandKind::OpenSshConnect(input)) if input.trim().is_empty() => {}
                    Some(kind) => cx.emit(PaletteEvent::Confirm(kind)),
                    None => cx.emit(PaletteEvent::Dismiss),
                }
            }
            ListEvent::Cancel => {
                if self.menu != PaletteMenu::Root {
                    self.menu = PaletteMenu::Root;
                    let root = self.root.clone();
                    let list = Self::build_root_list(root, window, cx);
                    self._sub = cx.subscribe_in(&list, window, Self::on_list_event);
                    self.list = list;
                    cx.notify();
                } else {
                    cx.emit(PaletteEvent::Dismiss);
                }
            }
            ListEvent::Select(_) => {}
        }
    }
}

impl EventEmitter<PaletteEvent> for PaletteView {}

const PALETTE_ROW_H: f32 = 30.;
const PALETTE_VISIBLE_ROWS: f32 = 12.;
const RECENT_ROWS: usize = 5;

impl Render for PaletteView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (border, popover) = (theme.border, theme.popover);
        let scrim = crate::ui::presets::scrim_fill(cx);

        let list_max_h = px(PALETTE_ROW_H * PALETTE_VISIBLE_ROWS + 4.);
        let card = v_flex()
            .w(px(560.))
            .bg(popover)
            .border_1()
            .border_color(border)
            .rounded(px(10.))
            .shadow_xl()
            .overflow_hidden()
            .pb_1()
            .child(
                List::new(&self.list)
                    .search_placeholder(self.search_placeholder())
                    .py_1()
                    .max_h(list_max_h),
            );

        div()
            .absolute()
            .inset_0()
            .flex()
            .items_start()
            .justify_center()
            .pt(px(120.))
            .bg(scrim)
            .on_key_down(cx.listener(|this, ev: &gpui::KeyDownEvent, _window, cx| {
                let ks = &ev.keystroke;
                let is_edit_gesture = (ks.key == "enter" && ks.modifiers.platform)
                    || (ks.key == "right" && !ks.modifiers.platform);
                if is_edit_gesture {
                    if let Some(edit) = this.selected_edit_command(cx) {
                        cx.stop_propagation();
                        cx.emit(PaletteEvent::Confirm(edit));
                    }
                }
            }))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|_this, _: &MouseDownEvent, _window, cx| {
                    cx.emit(PaletteEvent::Dismiss);
                }),
            )
            .child(div().occlude().child(card))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row_titles(query: &str) -> Vec<String> {
        PaletteDelegate::quick_connect_commands(query)
            .into_iter()
            .map(|c| c.title)
            .collect()
    }

    #[test]
    fn bare_word_gets_no_quick_connect_rows() {
        assert!(row_titles("java").is_empty());
        assert!(row_titles("split").is_empty());
        assert!(row_titles("").is_empty());
    }

    #[test]
    fn host_like_queries_get_connect_and_save_rows() {
        for q in [
            "deploy@10.0.0.5",
            "host.example.com",
            "java:2222",
            "ssh://java",
            "[::1]:2222",
        ] {
            let titles = row_titles(q);
            assert_eq!(
                titles,
                vec![
                    format!("Connect to \"{q}\""),
                    format!("Save \"{q}\" as profile…"),
                ],
                "query {q:?}"
            );
        }
    }

    #[test]
    fn host_like_but_unparsable_gets_no_rows() {
        assert!(row_titles("java:99999").is_empty());
        assert!(row_titles("@").is_empty());
    }

    #[test]
    fn empty_query_matches_everything() {
        assert_eq!(fuzzy_score("", "anything"), Some(0));
    }

    #[test]
    fn non_subsequence_does_not_match() {
        assert_eq!(fuzzy_score("zzz", "Split Right"), None);
        assert_eq!(fuzzy_score("thgir", "Split Right"), None);
    }

    #[test]
    fn word_initials_outrank_scattered_letters() {
        let target = fuzzy_score("sr", "Split Right").expect("matches");
        let scattered = fuzzy_score("sr", "SSH: Manage Profiles…").expect("matches");
        assert!(
            target > scattered,
            "expected 'Split Right' ({target}) to outrank 'SSH: Manage Profiles…' ({scattered})"
        );
    }

    #[test]
    fn exact_and_prefix_beat_mid_string() {
        let exact = fuzzy_score("copy", "Copy").expect("matches");
        let longer = fuzzy_score("copy", "Copy Working Directory").expect("matches");
        assert!(
            exact > longer,
            "expected exact 'Copy' ({exact}) above 'Copy Working Directory' ({longer})"
        );
    }

    #[test]
    fn subtitle_matches_are_found_but_discounted() {
        let cmd = Command::new("prod-web", CommandKind::NewTab)
            .with_subtitle("deploy@10.0.0.5".to_string());
        assert!(command_score("10.0.0", &cmd).is_some());
        let title_hit = command_score("prod", &cmd).expect("title matches");
        let subtitle_hit = command_score("deploy", &cmd).expect("subtitle matches");
        assert!(title_hit > subtitle_hit);
    }

    #[test]
    fn stable_ids_are_unique() {
        let mut seen = std::collections::HashSet::new();
        for kind in [
            CommandKind::NewTab,
            CommandKind::SplitRight,
            CommandKind::ClearTerminal,
            CommandKind::CopyText,
            CommandKind::CutText,
            CommandKind::PasteText,
            CommandKind::SelectAllText,
            CommandKind::FindInTerminal,
            CommandKind::FindNext,
            CommandKind::FindPrevious,
            CommandKind::OpenSettings,
            CommandKind::ShowKeyboardShortcuts,
            CommandKind::About,
            CommandKind::Quit,
            CommandKind::ShowRightPanel(RightPanelTab::Info),
            CommandKind::ShowRightPanel(RightPanelTab::Files),
        ] {
            let id = kind.id().expect("static command has an id");
            assert!(seen.insert(id), "duplicate command id {id:?}");
        }
    }

    #[test]
    fn dynamic_commands_have_no_id() {
        assert!(CommandKind::ActivateTab(2).id().is_none());
        assert!(CommandKind::SetTheme(0).id().is_none());
        assert!(CommandKind::QuickConnect("a@b".into()).id().is_none());
    }
}
