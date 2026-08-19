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
use crate::ui::i18n::{L10nKey, alias_translations, t, t_fmt};

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
    ToggleDocumentFill,
    DocumentWidthThird,
    DocumentWidthHalf,
    DocumentWidthTwoThirds,
    RestartSshSession,
    ScmCommit,
    ScmStageAll,
    ScmUnstageAll,
    ScmDiscardAll,
    ScmPush,
    ScmPull,
    ScmFetch,
    ScmSync,
    ScmCreateBranch,
    OpenBranchPicker,
    /// One branch, filled in by the picker. Dynamic like `OpenSshConnect`, so
    /// it gets no stable id and no key spec. Nothing emits it until the picker
    /// can list refs.
    #[allow(dead_code)]
    CheckoutBranch(String),
    ToggleDiffViewMode,
    SendSelectionToAgent,
    SendGitDiffToAgent,
    OpenThemePicker,
    OpenSshConnectInput,
    OpenSshConnect(String),
    SetTheme(usize),
    ActivateTab(usize),
    ConnectSavedProfile(Uuid),
    EditSavedProfile(Uuid),
    SaveSshSessionAsHost,
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
            // Frecency is keyed by this string, so it stays `right-panel-changes`
            // even though the panel is now called Source Control.
            ShowRightPanel(RightPanelTab::Scm) => "right-panel-changes",
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
            ToggleDocumentFill => "document-fill",
            DocumentWidthThird => "document-width-third",
            DocumentWidthHalf => "document-width-half",
            DocumentWidthTwoThirds => "document-width-two-thirds",
            RestartSshSession => "ssh-reconnect",
            ScmCommit => "git-commit",
            ScmStageAll => "git-stage-all",
            ScmUnstageAll => "git-unstage-all",
            ScmDiscardAll => "git-discard-all",
            ScmPush => "git-push",
            ScmPull => "git-pull",
            ScmFetch => "git-fetch",
            ScmSync => "git-sync",
            ScmCreateBranch => "git-create-branch",
            OpenBranchPicker => "git-checkout",
            ToggleDiffViewMode => "diff-view-mode",
            SendSelectionToAgent => "agent-send-selection",
            SendGitDiffToAgent => "agent-send-diff",
            OpenThemePicker => "change-theme",
            OpenSshConnectInput => "ssh-add-connection",
            OpenSshProfiles => "ssh-manage-profiles",
            SaveSshSessionAsHost => "ssh-save-connection",
            OpenSshConnect(_)
            | CheckoutBranch(_)
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
                RightPanelTab::Scm => "ShowRightPanelChanges",
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
            ToggleDocumentFill => "ToggleDocumentFill",
            DocumentWidthThird => "DocumentWidthThird",
            DocumentWidthHalf => "DocumentWidthHalf",
            DocumentWidthTwoThirds => "DocumentWidthTwoThirds",
            RestartSshSession => "RestartSshSession",
            OpenSshProfiles => "OpenSshProfiles",
            ScmCommit => "ScmCommit",
            ScmStageAll => "ScmStageAll",
            ScmUnstageAll => "ScmUnstageAll",
            ScmDiscardAll => "ScmDiscardAll",
            ScmPush => "ScmPush",
            ScmPull => "ScmPull",
            ScmFetch => "ScmFetch",
            ScmSync => "ScmSync",
            ScmCreateBranch => "ScmCreateBranch",
            OpenBranchPicker => "ScmCheckoutBranch",
            ToggleDiffViewMode => "ToggleDiffViewMode",
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
            | CheckoutBranch(_)
            | SetTheme(_)
            | ActivateTab(_)
            | ConnectSavedProfile(_)
            | EditSavedProfile(_)
            | SaveSshSessionAsHost
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
    Git,
    Terminal,
    Ssh,
    Agents,
    Application,
}

impl CommandGroup {
    pub(crate) const ORDER: [CommandGroup; 8] = [
        CommandGroup::TabsPanes,
        CommandGroup::Workspaces,
        CommandGroup::View,
        CommandGroup::Git,
        CommandGroup::Terminal,
        CommandGroup::Ssh,
        CommandGroup::Agents,
        CommandGroup::Application,
    ];

    pub(crate) fn title(self) -> &'static str {
        match self {
            CommandGroup::TabsPanes => t(L10nKey::CmdGroupTabsPanes),
            CommandGroup::Workspaces => t(L10nKey::CmdGroupWorkspaces),
            CommandGroup::View => t(L10nKey::CmdGroupView),
            CommandGroup::Git => t(L10nKey::CmdGroupGit),
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
    /// Text the query may match but the row never shows: the same label as
    /// every other locale words it, plus the stable command id. Built once,
    /// with the entry — the filter runs on every keystroke.
    pub aliases: Vec<&'static str>,
    pub kind: CommandKind,
    pub group: CommandGroup,
}

impl Command {
    pub fn new(title: impl Into<String>, kind: CommandKind) -> Self {
        Self {
            title: title.into(),
            subtitle: None,
            // The id is English and hyphenated ("change-theme"), which is close
            // enough to what someone reaching past a translated label types.
            aliases: kind.id().into_iter().collect(),
            kind,
            group: CommandGroup::Application,
        }
    }

    /// A command whose label comes out of the locale table, and which can
    /// therefore also be found by the wording any other locale would show.
    pub fn localized(key: L10nKey, kind: CommandKind) -> Self {
        let mut cmd = Self::new(t(key), kind);
        cmd.aliases.extend(alias_translations(key));
        cmd
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
        let document_filled = cfg.document_layout == crate::core::config::DocumentLayout::Fill;

        let tabs = [
            Command::localized(L10nKey::CmdNewTab, NewTab),
            Command::localized(L10nKey::CmdNewWorktreeTab, NewWorktreeTab)
                .with_subtitle(t(L10nKey::CmdNewWorktreeTabSubtitle)),
            Command::localized(L10nKey::CmdRenameTab, RenameTab),
            Command::localized(L10nKey::CmdSplitRight, SplitRight),
            Command::localized(L10nKey::CmdSplitDown, SplitDown),
            Command::localized(L10nKey::CmdZoomPane, ToggleMaximizePane),
            Command::localized(L10nKey::CmdNextPane, NextPane),
            Command::localized(L10nKey::CmdPreviousPane, PrevPane),
            Command::localized(L10nKey::CmdFocusPaneLeft, FocusPaneLeft),
            Command::localized(L10nKey::CmdFocusPaneRight, FocusPaneRight),
            Command::localized(L10nKey::CmdFocusPaneUp, FocusPaneUp),
            Command::localized(L10nKey::CmdFocusPaneDown, FocusPaneDown),
            Command::localized(L10nKey::CmdResizePaneLeft, ResizePaneLeft),
            Command::localized(L10nKey::CmdResizePaneRight, ResizePaneRight),
            Command::localized(L10nKey::CmdResizePaneUp, ResizePaneUp),
            Command::localized(L10nKey::CmdResizePaneDown, ResizePaneDown),
            Command::localized(L10nKey::CmdSwapPaneNext, SwapPaneNext),
            Command::localized(L10nKey::CmdSwapPanePrevious, SwapPanePrev),
            Command::localized(L10nKey::CmdNextTab, NextTab),
            Command::localized(L10nKey::CmdPreviousTab, PrevTab),
            Command::localized(L10nKey::CmdCopyWorkingDirectory, CopyWorkingDirectory),
            Command::localized(L10nKey::CmdCopySessionId, CopyAgentSessionId)
                .with_subtitle(t(L10nKey::CmdCopySessionIdSubtitle)),
            Command::localized(L10nKey::CmdForkSession, ForkAgentSession)
                .with_subtitle(t(L10nKey::CmdForkSessionSubtitle)),
            Command::localized(L10nKey::CmdMarkTabAsUnread, MarkTabUnread),
            Command::localized(L10nKey::CmdClosePaneTab, ClosePane),
            Command::localized(L10nKey::CmdCloseOtherTabs, CloseOtherTabs),
            Command::localized(L10nKey::CmdCloseTabsToTheRight, CloseTabsToTheRight),
            Command::localized(L10nKey::CmdReopenClosedTab, ReopenClosedTab),
        ];

        let workspaces = [
            Command::localized(L10nKey::CmdNewWorkspace, NewWorkspace),
            Command::localized(L10nKey::CmdSwitchWorkspace, OpenWorkspacePicker),
            Command::localized(L10nKey::CmdRenameWorkspace, RenameWorkspace),
            Command::localized(L10nKey::CmdStopWorkspace, StopWorkspace)
                .with_subtitle(t(L10nKey::CmdStopWorkspaceSubtitle)),
            Command::localized(L10nKey::CmdDeleteWorkspace, DeleteWorkspace)
                .with_subtitle(t(L10nKey::CmdDeleteWorkspaceSubtitle)),
        ];

        let view = [
            Command::localized(
                if sidebar_hidden {
                    L10nKey::CmdShowLeftSidebar
                } else {
                    L10nKey::CmdHideLeftSidebar
                },
                ToggleLeftPanel,
            ),
            Command::localized(
                if right_panel_open {
                    L10nKey::CmdHideRightPanel
                } else {
                    L10nKey::CmdShowRightPanel
                },
                ToggleRightPanel,
            ),
            Command::localized(L10nKey::CmdShowCodePanel, ToggleCodePanel),
            Command::localized(
                if document_filled {
                    L10nKey::CmdDocumentDock
                } else {
                    L10nKey::CmdDocumentFill
                },
                ToggleDocumentFill,
            ),
            Command::localized(L10nKey::CmdDocumentWidthThird, DocumentWidthThird),
            Command::localized(L10nKey::CmdDocumentWidthHalf, DocumentWidthHalf),
            Command::localized(L10nKey::CmdDocumentWidthTwoThirds, DocumentWidthTwoThirds),
            Command::localized(
                if tab_bar_left {
                    L10nKey::CmdTabBarMoveToTop
                } else {
                    L10nKey::CmdTabBarMoveToLeftSidebar
                },
                ToggleTabSidebar,
            ),
            Command::localized(
                L10nKey::CmdRightPanelInfo,
                ShowRightPanel(RightPanelTab::Info),
            ),
            Command::localized(
                L10nKey::CmdRightPanelChanges,
                ShowRightPanel(RightPanelTab::Scm),
            ),
            Command::localized(
                L10nKey::CmdRightPanelFiles,
                ShowRightPanel(RightPanelTab::Files),
            ),
            Command::localized(L10nKey::CmdChangeTheme, OpenThemePicker),
            Command::localized(L10nKey::CmdResetFontSize, ResetFontSize),
            Command::localized(L10nKey::CmdEnterFullScreen, ToggleFullscreen),
            Command::localized(L10nKey::CmdToggleDiffViewMode, ToggleDiffViewMode),
        ];

        // Their own group rather than more entries under View: View is a list
        // of things to show and hide, and ten git verbs in it would drown that.
        let git = [
            Command::localized(L10nKey::CmdGitCommit, ScmCommit),
            Command::localized(L10nKey::CmdGitStageAll, ScmStageAll),
            Command::localized(L10nKey::CmdGitUnstageAll, ScmUnstageAll),
            Command::localized(L10nKey::CmdGitDiscardAll, ScmDiscardAll)
                .with_subtitle(t(L10nKey::CmdGitDiscardAllSubtitle)),
            Command::localized(L10nKey::CmdGitCheckoutTo, OpenBranchPicker),
            Command::localized(L10nKey::CmdGitCreateBranch, ScmCreateBranch),
            Command::localized(L10nKey::CmdGitSync, ScmSync)
                .with_subtitle(t(L10nKey::CmdGitSyncSubtitle)),
            Command::localized(L10nKey::CmdGitPush, ScmPush),
            Command::localized(L10nKey::CmdGitPull, ScmPull),
            Command::localized(L10nKey::CmdGitFetch, ScmFetch),
        ];

        let terminal = [
            Command::localized(L10nKey::CmdClearScrollback, ClearTerminal),
            Command::localized(L10nKey::CmdFindInTerminal, FindInTerminal),
            Command::localized(L10nKey::CmdFindNext, FindNext),
            Command::localized(L10nKey::CmdFindPrevious, FindPrevious),
            Command::localized(L10nKey::CmdCopy, CopyText),
            Command::localized(L10nKey::CmdCut, CutText),
            Command::localized(L10nKey::CmdPaste, PasteText),
            Command::localized(L10nKey::CmdSelectAll, SelectAllText),
        ];

        let ssh = [
            Command::localized(L10nKey::CmdSshAddConnection, OpenSshConnectInput),
            Command::localized(L10nKey::CmdSshManageProfiles, OpenSshProfiles),
            Command::localized(L10nKey::CmdSshReconnect, RestartSshSession),
            Command::localized(L10nKey::CmdSshRemoteFiles, ToggleSftp),
            Command::localized(L10nKey::CmdSshPortForwarding, ShowSshForwards),
        ];

        let agents = [
            Command::localized(L10nKey::CmdAgentSendSelection, SendSelectionToAgent)
                .with_subtitle(t(L10nKey::CmdAgentSendSelectionSubtitle)),
            Command::localized(L10nKey::CmdAgentSendGitDiffForReview, SendGitDiffToAgent)
                .with_subtitle(t(L10nKey::CmdAgentSendGitDiffSubtitle)),
        ];

        let application = [
            Command::localized(L10nKey::CmdSettings, OpenSettings),
            Command::localized(L10nKey::CmdKeyboardShortcuts, ShowKeyboardShortcuts),
            Command::localized(L10nKey::CmdAboutTty7, About),
            Command::localized(L10nKey::CmdCheckForUpdates, CheckForUpdates),
            Command::localized(L10nKey::CmdDocumentation, OpenDocumentation),
            Command::localized(L10nKey::CmdJoinDiscord, OpenDiscord),
            Command::localized(L10nKey::CmdReportIssue, ReportIssue),
            Command::localized(L10nKey::CmdRestartServer, RestartDaemon)
                .with_subtitle(t(L10nKey::CmdRestartServerSubtitle)),
            Command::localized(L10nKey::CmdQuitTty7, Quit)
                .with_subtitle(t(L10nKey::CmdQuitTty7Subtitle)),
        ];

        let mut out = Vec::new();
        let mut push = |cmds: Vec<Command>, group: CommandGroup| {
            out.extend(cmds.into_iter().map(|c| c.in_group(group)));
        };
        push(tabs.into(), CommandGroup::TabsPanes);
        push(workspaces.into(), CommandGroup::Workspaces);
        push(view.into(), CommandGroup::View);
        push(git.into(), CommandGroup::Git);
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

    /// Row of the preset already in use, so the theme picker can open on it
    /// instead of previewing something else the moment it is opened.
    pub fn active_theme_index(cx: &App) -> Option<usize> {
        let active = crate::ui::theme::effective_preset_id(cx);
        crate::ui::presets::all(cx)
            .iter()
            .position(|p| p.id == active)
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

/// How far behind the visible label an alias hit lands. An alias *is* the
/// command's own name, only in another language, so the gap is small — but two
/// commands that both match must still be ordered by the text on screen.
const ALIAS_PENALTY: i32 = 10;

fn command_score(query: &str, cmd: &Command) -> Option<i32> {
    let title = fuzzy_score(query, &cmd.title);
    let subtitle = cmd
        .subtitle
        .as_deref()
        .and_then(|s| fuzzy_score(query, s))
        .map(|s| s / 2 - 25);
    // Aliases only ever decide whether a row is in the list and where it sits.
    // The row keeps rendering `title` untouched, so a hit on wording the user
    // cannot see can never end up underlined against the wrong characters.
    let alias = cmd
        .aliases
        .iter()
        .filter_map(|a| fuzzy_score(query, a))
        .max()
        .map(|s| s - ALIAS_PENALTY);
    [title, subtitle, alias].into_iter().flatten().max()
}

/// A bounded nudge, not a re-ranking. Two commands that match the query about
/// equally well should come out in the order they actually get run, but a
/// command used daily must not outrank a plainly better match — a single extra
/// matched character is worth 16 before bonuses.
fn frecency_bonus(used: f64) -> i32 {
    const CEILING: f64 = 24.0;
    if used <= 0.0 {
        return 0;
    }
    (used.ln_1p() * 8.0).min(CEILING).round() as i32
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
    /// Whether this is the root list: a typed address offers to connect to it,
    /// and an empty query falls back to the grouped command list.
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
        // Promoting a command to Recent moves it; it does not clone it. Leaving
        // it in its group too meant the five rows you use most were the five
        // rows the list showed twice.
        let promoted: Vec<&str> = recent.iter().filter_map(|(_, c)| c.kind.id()).collect();
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
                .filter(|c| !c.kind.id().is_some_and(|id| promoted.contains(&id)))
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
        window: &mut Window,
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
            let cfg = cx.global::<Config>();
            let now = crate::core::config::unix_now();
            let mut scored: Vec<(i32, Command)> = self
                .commands
                .iter()
                .filter_map(|c| {
                    let score = command_score(query, c)?;
                    // Frecency ordered the zero-query list and was then thrown
                    // away the moment a character was typed, so the command
                    // someone runs every day stopped floating exactly when
                    // they started reaching for it.
                    let used = c
                        .kind
                        .id()
                        .and_then(|id| cfg.command_frecency.get(id))
                        .map(|u| u.score(now))
                        .unwrap_or(0.0);
                    Some((score + frecency_bonus(used), c.clone()))
                })
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
        // Through `set_selected_index`, not by hand: the row index may not have
        // moved, but the command under it has, and the theme picker previews
        // the command — not the index.
        self.selected = None;
        let first = self.first_row();
        self.set_selected_index(first, window, cx);
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
                .px(px(PALETTE_LABEL_INSET))
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
        // The SSH hint only makes sense where typing user@host would actually
        // connect — the root menu. A theme picker with no matches teaching SSH
        // is a crossed wire (#602).
        let hint = if self.quick_connect_root {
            t(crate::ui::i18n::L10nKey::ConnectSshHint)
        } else {
            t(crate::ui::i18n::L10nKey::PaletteTryDifferentSearch)
        };
        v_flex()
            .py_8()
            .gap_1()
            .items_center()
            .text_sm()
            .text_color(cx.theme().muted_foreground)
            .child(crate::ui::i18n::t(
                crate::ui::i18n::L10nKey::NoMatchingCommands,
            ))
            .child(div().text_xs().child(hint))
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
                h_flex()
                    .items_center()
                    .gap_1()
                    .text_xs()
                    .text_color(muted)
                    .child(crate::ui::i18n::t(crate::ui::i18n::L10nKey::EditHint))
                    .child(crate::ui::keymap::key_tokens(EDIT_GESTURE).join("")),
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
                .mx(px(PALETTE_ROW_MX))
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
        let moved = self.selected != ix;
        self.selected = ix;
        // The list only emits `Select` for the arrow keys; a query that re-arms
        // the first row moves the highlight silently. The theme picker previews
        // whatever is highlighted, so it has to hear about both.
        if moved && let Some(ix) = ix {
            cx.emit(ListEvent::Select(ix));
        }
        cx.notify();
    }
}

pub enum PaletteEvent {
    Confirm(CommandKind),
    Dismiss,
    /// Show the theme at this preset index without persisting it: the theme
    /// picker previews the highlighted row while it stays open.
    PreviewTheme(usize),
    /// Put back the theme that was live before the preview started.
    CancelThemePreview,
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
    /// Preset index the theme picker is currently previewing, so the same
    /// theme is not re-applied on every redundant selection event.
    previewing: Option<usize>,
    _sub: Subscription,
}

impl PaletteView {
    pub fn new(commands: Vec<Command>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        Self::seeded(commands, "", window, cx)
    }

    /// The palette, opened with something already typed into it.
    ///
    /// For the callers that know what part of the palette they mean. A menu
    /// row leading here has already narrowed the question down to one group,
    /// and arriving at the unfiltered list would make the reader ask it again.
    ///
    /// The seed goes through the search field rather than around it, so what
    /// the reader sees is a palette in the state they would have typed it
    /// into: the text is there, selected rows are ranked against it, and the
    /// next keystroke goes on refining instead of starting over.
    pub fn seeded(
        commands: Vec<Command>,
        query: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let list = Self::build_root_list(commands.clone(), window, cx);
        if !query.is_empty() {
            list.update(cx, |state, cx| {
                state.set_query(query, window, cx);
            });
        }
        let _sub = cx.subscribe_in(&list, window, Self::on_list_event);
        Self {
            list,
            root: commands,
            menu: PaletteMenu::Root,
            previewing: None,
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
        let first = delegate.first_row();
        let list = cx.new(|cx| ListState::new(delegate, window, cx).searchable(true));
        list.update(cx, |state, cx| {
            // `ListState::new` starts with nothing selected, and it only picks a
            // row once a query changes. Opening the palette and pressing Return
            // therefore did nothing at all, and until then no row showed what
            // Return was aimed at. Every palette anywhere else arms the first
            // row on open.
            state.set_selected_index(first, window, cx);
            state.focus(window, cx);
        });
        list
    }

    fn show(
        &mut self,
        commands: Vec<Command>,
        selected: Option<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let list = Self::build_list(commands, window, cx);
        if let Some(row) = selected {
            list.update(cx, |state, cx| {
                state.set_selected_index(Some(IndexPath::new(row)), window, cx);
                state.scroll_to_selected_item(window, cx);
            });
        }
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
                        // Open on the theme already in use: the picker previews
                        // the highlighted row, and merely opening it must not
                        // change what the window looks like.
                        self.previewing = Command::active_theme_index(cx);
                        self.show(themes, self.previewing, window, cx);
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
                    if self.menu == PaletteMenu::Theme {
                        // Backing out of the picker is not a choice: whatever
                        // was previewed goes back to what it was.
                        self.previewing = None;
                        cx.emit(PaletteEvent::CancelThemePreview);
                    }
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
            ListEvent::Select(ix) => {
                if self.menu == PaletteMenu::Theme
                    && let Some(CommandKind::SetTheme(i)) = list.read(cx).delegate().command_at(*ix)
                    && self.previewing != Some(i)
                {
                    self.previewing = Some(i);
                    cx.emit(PaletteEvent::PreviewTheme(i));
                }
            }
        }
    }
}

impl EventEmitter<PaletteEvent> for PaletteView {}

const PALETTE_ROW_H: f32 = 30.;

/// Left inset of a row's *label*, so a section header can start on the same
/// pixel column as the rows it introduces. A row is a `ListItem` inset by
/// `PALETTE_ROW_MX` whose own padding is `px_3`; a header has neither, so it
/// has to carry the sum itself.
const PALETTE_ROW_MX: f32 = 5.;
const PALETTE_LABEL_INSET: f32 = PALETTE_ROW_MX + 12.;

/// The chord that opens the selected row for editing instead of running it.
///
/// It cannot be `→`: gpui-component's `Input` binds bare `right` to MoveRight
/// in its own key context, so with the query field focused the palette never
/// sees the key — the old `→ edit` badge was advertising a gesture that could
/// not fire. `⌘↵` is no better; the app binds it to ToggleFullscreen, which
/// wins for the same reason. `secondary-e` is claimed by neither.
const EDIT_GESTURE: &str = "secondary-e";

/// Matches `EDIT_GESTURE` against a live keystroke. Keep the two in step.
fn is_edit_gesture(ks: &gpui::Keystroke) -> bool {
    if ks.key != "e" {
        return false;
    }
    let m = &ks.modifiers;
    let secondary = if cfg!(target_os = "macos") {
        m.platform
    } else {
        m.control
    };
    secondary && !m.shift && !m.alt
}
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
            .key_context("Palette")
            .on_key_down(cx.listener(|this, ev: &gpui::KeyDownEvent, _window, cx| {
                let ks = &ev.keystroke;
                if is_edit_gesture(ks) {
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
    fn frecency_nudges_without_overruling_the_match() {
        assert_eq!(frecency_bonus(0.0), 0, "an unused command gets nothing");
        assert!(frecency_bonus(1.0) > 0, "one use is worth something");
        // Monotone in usage.
        assert!(frecency_bonus(20.0) > frecency_bonus(1.0));
        // Bounded: never worth more than one and a half matched characters,
        // so a command run a thousand times still loses to a better match.
        assert!(
            frecency_bonus(1_000.0) <= 24,
            "frecency is a tiebreak, not a ranking"
        );
        assert_eq!(frecency_bonus(1_000.0), frecency_bonus(10_000.0));
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
    fn a_translated_command_is_still_found_by_its_english_name() {
        crate::ui::i18n::set_locale("zh-CN");
        let cmd = Command::localized(L10nKey::CmdChangeTheme, CommandKind::OpenThemePicker);
        assert!(
            !cmd.title.to_lowercase().contains("theme"),
            "the row shows the Chinese label: {:?}",
            cmd.title
        );
        assert!(
            command_score("theme", &cmd).is_some(),
            "an English query must find the Chinese-labelled row"
        );
        assert!(
            command_score("テーマ", &cmd).is_some(),
            "so must the Japanese one"
        );
        // The displayed label still works, and still wins: the same query
        // against the locale that shows it scores higher.
        let zh = command_score("更改主题", &cmd).expect("the shown label matches");
        crate::ui::i18n::set_locale("en");
        let en = Command::localized(L10nKey::CmdChangeTheme, CommandKind::OpenThemePicker);
        assert!(zh > 0 && en.title.contains("Theme"));
        assert!(
            command_score("theme", &en).unwrap() > command_score("theme", &cmd).unwrap(),
            "a hit on the visible label must outrank the same hit on an alias"
        );
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

    /// The picker previews whatever row is highlighted, so it has to open on
    /// the row of the theme already in use — the one carrying the check mark.
    #[gpui::test]
    fn the_theme_picker_opens_on_the_theme_in_use(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let themes = crate::ui::presets::all(cx);
            let last = themes.last().expect("built-in themes").id.clone();
            cx.set_global(Config(crate::core::config::CoreConfig {
                theme_follow_system: false,
                theme_preset: last,
                ..Default::default()
            }));

            let ix = Command::active_theme_index(cx).expect("the live preset is listed");
            assert_eq!(ix, themes.len() - 1);
            let rows = Command::theme_commands(cx);
            assert!(
                rows[ix].title.ends_with('✓'),
                "row {ix} ({:?}) should be the checked one",
                rows[ix].title
            );
        });
    }

    #[test]
    fn dynamic_commands_have_no_id() {
        assert!(CommandKind::ActivateTab(2).id().is_none());
        assert!(CommandKind::SetTheme(0).id().is_none());
        assert!(CommandKind::QuickConnect("a@b".into()).id().is_none());
    }
}

#[cfg(test)]
mod gpui_tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    fn every_palette_command_has_a_stable_id(cx: &mut TestAppContext) {
        crate::core::config::pin_test_config_dir();
        cx.update(|cx| {
            cx.set_global(Config::default());
            crate::ui::i18n::set_locale("en");
            let chrome = ChromeState {
                rail_collapsed: false,
                right_panel_visible: false,
            };
            let mut seen = std::collections::HashSet::new();
            for cmd in Command::base_commands(cx, chrome) {
                // Frecency is keyed by this string. A command without one is
                // never learned, so it never rises in the list no matter how
                // often it is run.
                let id = cmd
                    .kind
                    .id()
                    .unwrap_or_else(|| panic!("`{}` has no stable id", cmd.title));
                assert!(seen.insert(id), "two commands claim the id {id:?}");
            }
        });
    }

    #[gpui::test]
    fn the_git_group_is_its_own_section(cx: &mut TestAppContext) {
        crate::core::config::pin_test_config_dir();
        cx.update(|cx| {
            cx.set_global(Config::default());
            crate::ui::i18n::set_locale("en");
            let chrome = ChromeState {
                rail_collapsed: false,
                right_panel_visible: false,
            };
            let cmds = Command::base_commands(cx, chrome);
            let git = cmds.iter().filter(|c| c.group == CommandGroup::Git).count();
            assert_eq!(git, 10, "the git section should hold ten verbs");
            // View stays a list of things to show and hide.
            assert!(
                !cmds.iter().any(|c| c.group == CommandGroup::View
                    && c.kind.id().unwrap_or("").starts_with("git-")),
            );
        });
    }
}
