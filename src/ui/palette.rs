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
use crate::ui::i18n::{L10nKey, t, t_fmt};

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
            CommandGroup::TabsPanes => t(L10nKey::CmdGroupTabsPanes),
            CommandGroup::Workspaces => t(L10nKey::CmdGroupWorkspaces),
            CommandGroup::View => t(L10nKey::CmdGroupView),
            CommandGroup::Terminal => t(L10nKey::CmdGroupTerminal),
            CommandGroup::Ssh => t(L10nKey::CmdGroupSsh),
            CommandGroup::Agents => t(L10nKey::CmdGroupAgents),
            CommandGroup::Application => t(L10nKey::CmdGroupApplication),
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
            Command::new(t(L10nKey::CmdNewTab), NewTab),
            Command::new(t(L10nKey::CmdNewWorktreeTab), NewWorktreeTab)
                .with_subtitle(t(L10nKey::CmdNewWorktreeTabSubtitle)),
            Command::new(t(L10nKey::CmdRenameTab), RenameTab),
            Command::new(t(L10nKey::CmdSplitRight), SplitRight),
            Command::new(t(L10nKey::CmdSplitDown), SplitDown),
            Command::new(t(L10nKey::CmdZoomPane), ToggleMaximizePane),
            Command::new(t(L10nKey::CmdNextPane), NextPane),
            Command::new(t(L10nKey::CmdPreviousPane), PrevPane),
            Command::new(t(L10nKey::CmdFocusPaneLeft), FocusPaneLeft),
            Command::new(t(L10nKey::CmdFocusPaneRight), FocusPaneRight),
            Command::new(t(L10nKey::CmdFocusPaneUp), FocusPaneUp),
            Command::new(t(L10nKey::CmdFocusPaneDown), FocusPaneDown),
            Command::new(t(L10nKey::CmdResizePaneLeft), ResizePaneLeft),
            Command::new(t(L10nKey::CmdResizePaneRight), ResizePaneRight),
            Command::new(t(L10nKey::CmdResizePaneUp), ResizePaneUp),
            Command::new(t(L10nKey::CmdResizePaneDown), ResizePaneDown),
            Command::new(t(L10nKey::CmdSwapPaneNext), SwapPaneNext),
            Command::new(t(L10nKey::CmdSwapPanePrevious), SwapPanePrev),
            Command::new(t(L10nKey::CmdNextTab), NextTab),
            Command::new(t(L10nKey::CmdPreviousTab), PrevTab),
            Command::new(t(L10nKey::CmdCopyWorkingDirectory), CopyWorkingDirectory),
            Command::new(t(L10nKey::CmdCopySessionId), CopyAgentSessionId)
                .with_subtitle(t(L10nKey::CmdCopySessionIdSubtitle)),
            Command::new(t(L10nKey::CmdForkSession), ForkAgentSession)
                .with_subtitle(t(L10nKey::CmdForkSessionSubtitle)),
            Command::new(t(L10nKey::CmdMarkTabAsUnread), MarkTabUnread),
            Command::new(t(L10nKey::CmdClosePaneTab), ClosePane),
            Command::new(t(L10nKey::CmdCloseOtherTabs), CloseOtherTabs),
            Command::new(t(L10nKey::CmdCloseTabsToTheRight), CloseTabsToTheRight),
            Command::new(t(L10nKey::CmdReopenClosedTab), ReopenClosedTab),
        ];

        let workspaces = [
            Command::new(t(L10nKey::CmdNewWorkspace), NewWorkspace),
            Command::new(t(L10nKey::CmdSwitchWorkspace), OpenWorkspacePicker),
            Command::new(t(L10nKey::CmdRenameWorkspace), RenameWorkspace),
            Command::new(t(L10nKey::CmdStopWorkspace), StopWorkspace)
                .with_subtitle(t(L10nKey::CmdStopWorkspaceSubtitle)),
            Command::new(t(L10nKey::CmdDeleteWorkspace), DeleteWorkspace)
                .with_subtitle(t(L10nKey::CmdDeleteWorkspaceSubtitle)),
        ];

        let view = [
            Command::new(
                if sidebar_hidden {
                    t(L10nKey::CmdShowLeftSidebar)
                } else {
                    t(L10nKey::CmdHideLeftSidebar)
                },
                ToggleLeftPanel,
            ),
            Command::new(
                if right_panel_open {
                    t(L10nKey::CmdHideRightPanel)
                } else {
                    t(L10nKey::CmdShowRightPanel)
                },
                ToggleRightPanel,
            ),
            Command::new(t(L10nKey::CmdShowCodePanel), ToggleCodePanel),
            Command::new(
                if tab_bar_left {
                    t(L10nKey::CmdTabBarMoveToTop)
                } else {
                    t(L10nKey::CmdTabBarMoveToLeftSidebar)
                },
                ToggleTabSidebar,
            ),
            Command::new(
                t(L10nKey::CmdRightPanelInfo),
                ShowRightPanel(RightPanelTab::Info),
            ),
            Command::new(
                t(L10nKey::CmdRightPanelOutline),
                ShowRightPanel(RightPanelTab::Outline),
            ),
            Command::new(
                t(L10nKey::CmdRightPanelChanges),
                ShowRightPanel(RightPanelTab::Changes),
            ),
            Command::new(
                t(L10nKey::CmdRightPanelFiles),
                ShowRightPanel(RightPanelTab::Files),
            ),
            Command::new(t(L10nKey::CmdChangeTheme), OpenThemePicker),
            Command::new(t(L10nKey::CmdResetFontSize), ResetFontSize),
            Command::new(t(L10nKey::CmdEnterFullScreen), ToggleFullscreen),
        ];

        let terminal = [
            Command::new(t(L10nKey::CmdClearScrollback), ClearTerminal),
            Command::new(t(L10nKey::CmdFindInTerminal), FindInTerminal),
            Command::new(t(L10nKey::CmdFindNext), FindNext),
            Command::new(t(L10nKey::CmdFindPrevious), FindPrevious),
            Command::new(t(L10nKey::CmdCopy), CopyText),
            Command::new(t(L10nKey::CmdCut), CutText),
            Command::new(t(L10nKey::CmdPaste), PasteText),
            Command::new(t(L10nKey::CmdSelectAll), SelectAllText),
        ];

        let ssh = [
            Command::new(t(L10nKey::CmdSshAddConnection), OpenSshConnectInput),
            Command::new(t(L10nKey::CmdSshManageProfiles), OpenSshProfiles),
            Command::new(t(L10nKey::CmdSshReconnect), RestartSshSession),
            Command::new(t(L10nKey::CmdSshRemoteFiles), ToggleSftp),
            Command::new(t(L10nKey::CmdSshPortForwarding), ShowSshForwards),
        ];

        let agents = [
            Command::new(t(L10nKey::CmdAgentSendSelection), SendSelectionToAgent)
                .with_subtitle(t(L10nKey::CmdAgentSendSelectionSubtitle)),
            Command::new(t(L10nKey::CmdAgentSendGitDiffForReview), SendGitDiffToAgent)
                .with_subtitle(t(L10nKey::CmdAgentSendGitDiffSubtitle)),
        ];

        let application = [
            Command::new(t(L10nKey::CmdSettings), OpenSettings),
            Command::new(t(L10nKey::CmdKeyboardShortcuts), ShowKeyboardShortcuts),
            Command::new(t(L10nKey::CmdAboutTty7), About),
            Command::new(t(L10nKey::CmdCheckForUpdates), CheckForUpdates),
            Command::new(t(L10nKey::CmdDocumentation), OpenDocumentation),
            Command::new(t(L10nKey::CmdJoinDiscord), OpenDiscord),
            Command::new(t(L10nKey::CmdReportIssue), ReportIssue),
            Command::new(t(L10nKey::CmdRestartServer), RestartDaemon)
                .with_subtitle(t(L10nKey::CmdRestartServerSubtitle)),
            Command::new(t(L10nKey::CmdQuitTty7), Quit)
                .with_subtitle(t(L10nKey::CmdQuitTty7Subtitle)),
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
        let trimmed = input.trim();
        let title = if trimmed.is_empty() {
            t(L10nKey::CmdSshAddConnection).to_string()
        } else {
            t_fmt(L10nKey::CmdSshConnectWithInput, &[("input", trimmed)])
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
                title: Some(t(L10nKey::CmdRecent).into()),
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
                        t_fmt(L10nKey::CmdQuickConnect, &[("target", &target)]),
                        CommandKind::QuickConnect(target.clone()),
                    ),
                    Command::new(
                        t_fmt(L10nKey::CmdQuickConnectSaveProfile, &[("target", &target)]),
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
            .child(crate::ui::i18n::t(
                crate::ui::i18n::L10nKey::NoMatchingCommands,
            ))
            .child(
                div()
                    .text_xs()
                    .child(crate::ui::i18n::t(crate::ui::i18n::L10nKey::ConnectSshHint)),
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
            row = row.child(
                div()
                    .text_xs()
                    .text_color(muted)
                    .child(crate::ui::i18n::t(crate::ui::i18n::L10nKey::EditHint)),
            );
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
            PaletteMenu::Root => t(crate::ui::i18n::L10nKey::SearchCommandsOrHost),
            PaletteMenu::Theme => t(crate::ui::i18n::L10nKey::SearchTheme),
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
        crate::ui::i18n::set_locale("en");
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
                    t_fmt(L10nKey::CmdQuickConnect, &[("target", q)]),
                    t_fmt(L10nKey::CmdQuickConnectSaveProfile, &[("target", q)]),
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
